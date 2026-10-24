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

use crate::keycode::KeyCode;

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
    /// Keep the next word or letter lowercase (`{>}`, as in Lapwing's fingerspelling)
    LowercaseNext,
    /// A fingerspelled letter or group; joins a preceding fingerspelled letter
    Fingerspell(String),
    /// Capitalize the previous word
    RetroCapitalize,
    /// Remove the space before this point
    RetroDeleteSpace,
    /// Put a space before this point
    RetroInsertSpace,
    /// Lowercase the previous word's first letter (`{:retro_case:lower_first_char}`)
    RetroLowercase,
    /// Raw text glued to both neighbours with no spacing of its own (`{^ ^}`, `{^\n^}`)
    Glue(String),
    /// A key press, such as cursor movement or Enter (`{#Left}`, `{#Shift_l(Home)}`)
    Key(KeyPress),
}

/// One key press, with any modifiers held down while it is pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPress {
    pub modifiers: Vec<KeyCode>,
    pub key: KeyCode,
}

/// Parse the inside of a `{#...}` key command, such as `Left` or `Shift_l(Home)`.
fn parse_key_press(expr: &str) -> Option<KeyPress> {
    if let Some((name, rest)) = expr.split_once('(') {
        let inner = rest.strip_suffix(')')?;
        let modifier = modifier_key(name)?;
        let mut press = parse_key_press(inner)?;
        press.modifiers.insert(0, modifier);
        return Some(press);
    }
    Some(KeyPress {
        modifiers: Vec::new(),
        key: key_name(expr)?,
    })
}

/// Modifier names as Plover writes them, in either case.
fn modifier_key(name: &str) -> Option<KeyCode> {
    match name.to_ascii_lowercase().as_str() {
        "shift_l" => Some(KeyCode::KC_LSFT),
        "control_l" => Some(KeyCode::KC_LCTL),
        "alt_l" => Some(KeyCode::KC_LALT),
        _ => None,
    }
}

/// Key names as Plover writes them. Matching ignores case, so `Return` and `return` agree.
fn key_name(name: &str) -> Option<KeyCode> {
    let lower = name.to_ascii_lowercase();
    if let Some(number) = lower.strip_prefix('f') {
        return match number.parse::<u8>().ok()? {
            1 => Some(KeyCode::KC_F1),
            2 => Some(KeyCode::KC_F2),
            3 => Some(KeyCode::KC_F3),
            4 => Some(KeyCode::KC_F4),
            5 => Some(KeyCode::KC_F5),
            6 => Some(KeyCode::KC_F6),
            7 => Some(KeyCode::KC_F7),
            8 => Some(KeyCode::KC_F8),
            9 => Some(KeyCode::KC_F9),
            10 => Some(KeyCode::KC_F10),
            11 => Some(KeyCode::KC_F11),
            12 => Some(KeyCode::KC_F12),
            _ => None,
        };
    }
    match lower.as_str() {
        "left" => Some(KeyCode::KC_LEFT),
        "right" => Some(KeyCode::KC_RGHT),
        "up" => Some(KeyCode::KC_UP),
        "down" => Some(KeyCode::KC_DOWN),
        "home" => Some(KeyCode::KC_HOME),
        "end" => Some(KeyCode::KC_END),
        "page_up" => Some(KeyCode::KC_PGUP),
        "page_down" => Some(KeyCode::KC_PGDN),
        "delete" => Some(KeyCode::KC_DEL),
        "backspace" => Some(KeyCode::KC_BSPC),
        "escape" => Some(KeyCode::KC_ESC),
        "tab" => Some(KeyCode::KC_TAB),
        "return" => Some(KeyCode::KC_ENT),
        _ => None,
    }
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
    // Plover commands written without braces, such as `=undo`, are unsupported and skipped
    // rather than typed out
    if value.starts_with('=') {
        return None;
    }
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // `\{` and `\}` are literal braces, as in Lapwing's ch20 brace outlines
            '\\' if matches!(chars.peek(), Some('{' | '}')) => {
                text.extend(chars.next());
            }
            '{' => {
                if !text.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut text)));
                }
                let mut command = String::new();
                loop {
                    match chars.next()? {
                        '}' => break,
                        '{' => return None,
                        other => command.push(other),
                    }
                }
                pieces.extend(parse_command(&command)?);
            }
            '}' => return None,
            other => text.push(other),
        }
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    // `{}` on its own is a valid no-op outline, so only plain empty text is rejected
    (!pieces.is_empty() || value.contains('{')).then_some(pieces)
}

fn parse_command(command: &str) -> Option<Vec<Piece>> {
    // `~|` marks raw text (quotes, brackets, Enter). Its spacing comes from the
    // surrounding `^` marks, so the marker itself is dropped.
    let command = command.replace("~|", "");
    let command = command.as_str();
    match command {
        // An empty command is a no-op, as in Lapwing's `{}{:case:...}` outlines
        "" => Some(Vec::new()),
        "^" => Some(vec![Piece::Attach]),
        "-|" => Some(vec![Piece::CapNext]),
        ">" => Some(vec![Piece::LowercaseNext]),
        "*<" => Some(vec![Piece::RetroCapitalize]),
        "*!" => Some(vec![Piece::RetroDeleteSpace]),
        "*?" => Some(vec![Piece::RetroInsertSpace]),
        ":case:cap_first_word" => Some(vec![Piece::CapNext]),
        ":case:lower_first_char" => Some(vec![Piece::LowercaseNext]),
        ":retro_case:cap_first_word" => Some(vec![Piece::RetroCapitalize]),
        ":retro_case:lower_first_char" => Some(vec![Piece::RetroLowercase]),
        // Sentence-ending punctuation attaches to the previous word and capitalizes the next
        "." | "?" | "!" => Some(vec![Piece::AttachText(command.to_string()), Piece::CapNext]),
        // Other punctuation attaches without capitalizing
        "," | ";" | ":" => Some(vec![Piece::AttachText(command.to_string())]),
        _ => {
            if let Some(expr) = command.strip_prefix('#') {
                return parse_key_press(expr).map(|press| vec![Piece::Key(press)]);
            }
            if let Some(letters) = command.strip_prefix('&') {
                return (!letters.is_empty())
                    .then(|| vec![Piece::Fingerspell(letters.to_string())]);
            }
            // `{^text^}` glues on both sides, `{^text}` only before, `{text^}` only after.
            // Anything else is a command keymux doesn't know, so the entry is skipped.
            match (command.strip_prefix('^'), command.strip_suffix('^')) {
                (Some(_), Some(_)) if command.len() >= 2 => {
                    let inner = &command[1..command.len() - 1];
                    Some(vec![if inner.is_empty() {
                        Piece::Attach
                    } else {
                        Piece::Glue(inner.to_string())
                    }])
                }
                (Some(text), None) => {
                    (!text.is_empty()).then(|| vec![Piece::AttachText(text.to_string())])
                }
                (None, Some(text)) => {
                    (!text.is_empty()).then(|| vec![Piece::TextAttach(text.to_string())])
                }
                (None, None) => None,
                (Some(_), Some(_)) => None,
            }
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
            Piece::LowercaseNext => state.capitalize = false,
            Piece::RetroCapitalize
            | Piece::RetroDeleteSpace
            | Piece::RetroInsertSpace
            | Piece::RetroLowercase
            | Piece::Key(_) => {}
            Piece::Glue(text) => {
                out.push_str(text);
                state.has_text = true;
                state.suppress_space = true;
                state.last_fingerspell = false;
            }
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

/// Lowercase the first letter of `word`, leaving the rest alone.
#[must_use]
pub fn lowercase_word(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        let mut out: String = first.to_lowercase().collect();
        out.push_str(chars.as_str());
        out
    })
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
    fn lowercase_next_parses_and_equals_commands_are_skipped() {
        assert_eq!(
            parse_pieces("{>}{&n}"),
            Some(vec![Piece::LowercaseNext, Piece::Fingerspell("n".into())])
        );
        // Plover `=` commands are unsupported, so the entry is skipped, not typed out
        assert_eq!(parse_pieces("=undo"), None);
        assert_eq!(parse_pieces("=repeat_last_translation"), None);
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

    fn press(modifiers: &[KeyCode], key: KeyCode) -> Piece {
        Piece::Key(KeyPress {
            modifiers: modifiers.to_vec(),
            key,
        })
    }

    #[test]
    fn key_names_parse_in_either_case() {
        assert_eq!(parse_pieces("{#Left}"), Some(vec![press(&[], KeyCode::KC_LEFT)]));
        // Lapwing writes both `Return` and `return`
        assert_eq!(parse_pieces("{#Return}"), Some(vec![press(&[], KeyCode::KC_ENT)]));
        assert_eq!(parse_pieces("{#return}"), Some(vec![press(&[], KeyCode::KC_ENT)]));
        assert_eq!(parse_pieces("{#Page_Up}"), Some(vec![press(&[], KeyCode::KC_PGUP)]));
        assert_eq!(parse_pieces("{#F12}"), Some(vec![press(&[], KeyCode::KC_F12)]));
    }

    #[test]
    fn modifiers_wrap_the_key_they_hold_down() {
        assert_eq!(
            parse_pieces("{#Shift_l(Home)}"),
            Some(vec![press(&[KeyCode::KC_LSFT], KeyCode::KC_HOME)])
        );
        // Nested modifiers: Control and Shift together, outermost first
        assert_eq!(
            parse_pieces("{#Control_L(Shift_l(Left))}"),
            Some(vec![press(
                &[KeyCode::KC_LCTL, KeyCode::KC_LSFT],
                KeyCode::KC_LEFT
            )])
        );
    }

    #[test]
    fn unknown_keys_and_broken_modifiers_are_rejected() {
        assert_eq!(parse_pieces("{#Nope}"), None);
        assert_eq!(parse_pieces("{#F13}"), None);
        assert_eq!(parse_pieces("{#Shift_l(Left}"), None);
        assert_eq!(parse_pieces("{#Hyper_l(Left)}"), None);
    }
}
