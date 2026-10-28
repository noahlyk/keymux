//! RTF/CRE dictionaries: the Rich Text Format with Court Reporting Extensions that professional
//! steno software exchanges. A port of Plover's `rtfcre_parse.py` and its tokenizer.
//!
//! An entry is `{\*\cxs KAT}cat`: the outline in the same notation as everywhere else, then the
//! translation in RTF's own formatting controls, which are turned into Plover's translation
//! language (`\cxds` is delete-space, `{^...}`; `\cxfc` force-cap, `{-|}`; a `\cxp` group is
//! punctuation, `{.}`). What Plover can't read, keymux skips too: comments, conflict lists and
//! usage data.

use anyhow::{bail, Result};
use std::collections::HashMap;

/// Windows-1252, which RTF dictionaries are written in: bytes 0x80 to 0x9F are not Latin-1.
fn decode_cp1252(bytes: &[u8]) -> Result<String> {
    const HIGH: [Option<char>; 32] = [
        Some('\u{20ac}'),
        None,
        Some('\u{201a}'),
        Some('\u{192}'),
        Some('\u{201e}'),
        Some('\u{2026}'),
        Some('\u{2020}'),
        Some('\u{2021}'),
        Some('\u{2c6}'),
        Some('\u{2030}'),
        Some('\u{160}'),
        Some('\u{2039}'),
        Some('\u{152}'),
        None,
        Some('\u{17d}'),
        None,
        None,
        Some('\u{2018}'),
        Some('\u{2019}'),
        Some('\u{201c}'),
        Some('\u{201d}'),
        Some('\u{2022}'),
        Some('\u{2013}'),
        Some('\u{2014}'),
        Some('\u{2dc}'),
        Some('\u{2122}'),
        Some('\u{161}'),
        Some('\u{203a}'),
        Some('\u{153}'),
        None,
        Some('\u{17e}'),
        Some('\u{178}'),
    ];
    let mut text = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            0x80..=0x9f => match HIGH[usize::from(byte - 0x80)] {
                Some(c) => text.push(c),
                None => bail!("the file is not Windows-1252 text (byte {byte:#04x})"),
            },
            _ => text.push(char::from(byte)),
        }
    }
    Ok(text)
}

/// Splits RTF into tokens: `{`, `}`, control words and symbols (`\cxds`, `\u233`, `\*`, `\~`) and
/// runs of text. Raw newlines are not text.
struct Tokenizer {
    chars: Vec<char>,
    at: usize,
    /// Tokens put back, the last put back first
    rewound: Vec<Option<String>>,
}

impl Tokenizer {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            at: 0,
            rewound: Vec::new(),
        }
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.at + offset).copied()
    }

    /// The next token, or `None` at the end.
    fn next_token(&mut self) -> Option<String> {
        if let Some(token) = self.rewound.pop() {
            return token;
        }
        while matches!(self.peek(0), Some('\n' | '\r')) {
            self.at += 1;
        }
        let start = self.at;
        let first = self.peek(0)?;
        match first {
            '{' | '}' => self.at += 1,
            '\\' => {
                self.at += 1;
                while self.peek(0).is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.at += 1;
                }
                if self.at == start + 1 {
                    // A control symbol: the backslash and the character after it
                    if self.peek(0).is_some() {
                        self.at += 1;
                    }
                } else {
                    // A control word, with an optional number
                    if self.peek(0).is_some_and(|c| c == '-' || c.is_ascii_digit()) {
                        self.at += 1;
                        while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                            self.at += 1;
                        }
                    }
                    let end = self.at;
                    // One space after a control word belongs to it, and is not text
                    if self.peek(0) == Some(' ') {
                        self.at += 1;
                    }
                    return Some(self.chars[start..end].iter().collect());
                }
            }
            _ => {
                while self
                    .peek(0)
                    .is_some_and(|c| !matches!(c, '\\' | '{' | '}' | '\r' | '\n'))
                {
                    self.at += 1;
                }
            }
        }
        Some(self.chars[start..self.at].iter().collect())
    }

    fn rewind(&mut self, token: Option<String>) {
        self.rewound.push(token);
    }
}

/// A translation as written, tidied the way Plover does: punctuation CAT dictionaries leave
/// bare, and runs of spaces at either end, which would otherwise be lost.
fn finalize_translation(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    // caseCATalyst doesn't put punctuation in \cxp: any lone punctuation at the start is special
    if ".?!:;,".contains(chars[0]) && (chars.len() == 1 || (chars.len() == 2 && chars[1] == ' ')) {
        return format!("{{{}}}{}", chars[0], chars[1..].iter().collect::<String>());
    }
    let mut text = text.to_string();
    let left = text.chars().count() - text.trim_start().chars().count();
    if left > 1 {
        let (space, rest) = split_chars(&text, left);
        text = format!("{{^{space}^}}{rest}");
    }
    let right = text.chars().count() - text.trim_end().chars().count();
    if right > 1 {
        let (rest, space) = split_chars(&text, text.chars().count() - right);
        text = format!("{rest}{{^{space}^}}");
    }
    text
}

fn split_chars(text: &str, at: usize) -> (String, String) {
    (
        text.chars().take(at).collect(),
        text.chars().skip(at).collect(),
    )
}

fn is_style(name: &str) -> bool {
    name.strip_prefix('s')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// A `\uN` or `\uc` followed by a number: the control word's name and its number.
fn control_number<'a>(ctrl: &'a str, name: &str) -> Option<&'a str> {
    let number = ctrl.strip_prefix(name)?;
    let digits = number.strip_prefix('-').unwrap_or(number);
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(number)
}

fn plain_control(ctrl: &str) -> Option<&'static str> {
    Some(match ctrl {
        "*" => "",
        // Hard space and non-breaking hyphen
        "~" => "{^ ^}",
        "_" => "{^-^}",
        // An escaped newline is a paragraph
        "" | "\n" | "\r" | "par" => "\n\n",
        "\\" => "\\",
        "{" => "{",
        "}" => "}",
        "-" => "-",
        "line" => "\n",
        "tab" => "\t",
        // Force capital and force lower case
        "cxfc" => "{-|}",
        "cxfl" => "{>}",
        _ => return None,
    })
}

/// One group being read: its destination, the text so far, and how many characters follow a
/// `\u` escape as a fallback.
struct Group {
    destination: Option<String>,
    text: String,
    uc: usize,
}

/// Read an RTF/CRE dictionary: each outline with its translation, in Plover's translation
/// language. A dictionary that is damaged is read as far as it goes.
pub fn parse_rtf(bytes: &[u8]) -> Result<Vec<(String, String)>> {
    let text = decode_cp1252(bytes)?;
    let mut tokens = Tokenizer::new(&text);
    if tokens.next_token().as_deref() != Some("{")
        || tokens.next_token().as_deref() != Some("\\rtf1")
    {
        bail!("not an RTF dictionary: the header is wrong");
    }
    let mut entries = Vec::new();
    let mut current = Group {
        destination: Some("rtf1".into()),
        text: String::new(),
        uc: 1,
    };
    let mut skip_count = 0usize;
    let mut stack: Vec<Group> = Vec::new();
    let mut stylesheet: HashMap<String, String> = HashMap::new();
    let mut steno: Option<String> = None;
    // Text a closing group adds to its parent. Left as it was by groups that don't set it.
    let mut text = String::new();
    let mut pending_high: Option<u16> = None;

    while let Some(token) = tokens.next_token() {
        if token == "{" {
            let mut rewind = false;
            let mut ignored = false;
            let mut destination: Option<String> = None;
            let Some(mut next) = tokens.next_token() else {
                break;
            };
            if next == "\\*" {
                let Some(after) = tokens.next_token() else {
                    break;
                };
                next = after;
                ignored = true;
            }
            if let Some(name) = next.strip_prefix('\\') {
                destination = Some(name.to_string());
                if name == "cxs" {
                    if !stack.is_empty() {
                        // A new entry starts before the last was closed: close it for them
                        tokens.rewind(Some(next));
                        if ignored {
                            tokens.rewind(Some("\\*".into()));
                        }
                        tokens.rewind(Some("{".into()));
                        for _ in 0..stack.len() {
                            tokens.rewind(Some("}".into()));
                        }
                        continue;
                    }
                    if let Some(outline) = steno.take() {
                        entries.push((outline, finalize_translation(&current.text)));
                    }
                    ignored = false;
                    current.text.clear();
                } else if matches!(
                    name,
                    "cxfing" | "cxsvatdictflags" | "cxplovermacro" | "cxplovermeta"
                ) {
                    ignored = false;
                } else if !is_style(name) {
                    // E.g. `{\par`: `\par` is a control word of the text, not a destination
                    rewind = true;
                }
            } else {
                rewind = true;
            }
            if ignored {
                // Skip the content of a group nobody reads
                let mut depth = 1;
                loop {
                    match tokens.next_token().as_deref() {
                        None => break,
                        Some("{") => depth += 1,
                        Some("}") => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        Some(_) => {}
                    }
                }
                if depth > 0 {
                    break;
                }
                continue;
            }
            let uc = current.uc;
            stack.push(std::mem::replace(
                &mut current,
                Group {
                    destination,
                    text: String::new(),
                    uc,
                },
            ));
            skip_count = 0;
            if rewind {
                tokens.rewind(Some(next));
            }
            continue;
        }
        if token == "}" {
            let Some(parent) = stack.pop() else {
                match tokens.next_token() {
                    None => break,
                    Some(extra) => {
                        // Something after the end: ignored
                        tokens.rewind(Some(extra));
                        continue;
                    }
                }
            };
            match current.destination.as_deref() {
                Some("cxs") => {
                    steno = Some(current.text.clone());
                    text.clear();
                }
                Some("cxp") => {
                    let trimmed = current.text.trim();
                    text = match trimmed {
                        "." | "!" | "?" | "," | ";" | ":" => format!("{{{trimmed}}}"),
                        "'" => "{^'}".to_string(),
                        "-" | "/" => format!("{{^{trimmed}^}}"),
                        // Unknown punctuation is shown as given
                        _ => format!("{{^{}^}}", current.text),
                    };
                }
                Some("cxsvatdictflags") => {
                    text = if current.text.contains('N') {
                        "{-|}".into()
                    } else {
                        String::new()
                    };
                }
                Some("cxfing") => text = format!("{{&{}}}", current.text),
                Some("cxplovermacro") => text = format!("={}", current.text),
                Some("cxplovermeta") => text = format!("{{{}}}", current.text),
                Some(name)
                    if is_style(name) && parent.destination.as_deref() == Some("stylesheet") =>
                {
                    stylesheet.insert(name.to_string(), current.text.clone());
                }
                _ => text = current.text.clone(),
            }
            current = parent;
            skip_count = 0;
            current.text.push_str(&text);
            continue;
        }
        if let Some(ctrl) = token.strip_prefix('\\') {
            if let Some(plain) = plain_control(ctrl) {
                if skip_count > 0 {
                    skip_count -= 1;
                } else {
                    current.text.push_str(plain);
                }
            } else if let Some(number) = control_number(ctrl, "u") {
                let unit = number.parse::<i64>().unwrap_or(0) & 0xFFFF;
                let unit = unit as u16;
                if (0xD800..0xDC00).contains(&unit) {
                    pending_high = Some(unit);
                } else if (0xDC00..0xE000).contains(&unit) && pending_high.is_some() {
                    let high = u32::from(pending_high.take().unwrap_or(0xD800)) - 0xD800;
                    let code = 0x1_0000 + (high << 10) + (u32::from(unit) - 0xDC00);
                    current
                        .text
                        .push(char::from_u32(code).unwrap_or('\u{fffd}'));
                } else {
                    current
                        .text
                        .push(char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}'));
                }
                skip_count = current.uc;
            } else if let Some(number) = control_number(ctrl, "uc") {
                current.uc = number.parse().unwrap_or(1);
            } else if ctrl == "cxds" {
                let next = tokens.next_token();
                match next {
                    Some(word) if !word.starts_with(['\\', '{', '}']) => {
                        let after = tokens.next_token();
                        if after.as_deref() == Some("\\cxds") {
                            // Infix
                            current.text.push_str(&format!("{{^{word}^}}"));
                        } else {
                            // Prefix
                            current.text.push_str(&format!("{{^{word}}}"));
                            tokens.rewind(after);
                        }
                    }
                    other => {
                        current.text.push_str("{^}");
                        tokens.rewind(other);
                    }
                }
            } else if ctrl == "cxdstroke" {
                current.text = "=undo".to_string();
            } else if ctrl == "cxfing" {
                let next = tokens.next_token();
                match next {
                    Some(word) if !word.starts_with(['\\', '{', '}']) => {
                        current.text.push_str(&format!("{{&{word}}}"));
                    }
                    other => tokens.rewind(other),
                }
            } else if is_style(ctrl) {
                // caseCATalyst declares new styles without a paragraph before them
                if !current.text.ends_with("\n\n") {
                    current.text.push_str("\n\n");
                }
                // Indent continuation styles
                if stylesheet
                    .get(ctrl)
                    .is_some_and(|style| style.starts_with("Contin"))
                {
                    current.text.push_str("    ");
                }
            }
            continue;
        }
        // Text
        let mut piece = token;
        if skip_count > 0 {
            let skipped = skip_count.min(piece.chars().count());
            skip_count -= skipped;
            piece = piece.chars().skip(skipped).collect();
            if piece.is_empty() {
                continue;
            }
        }
        let after = tokens.next_token();
        if after.as_deref() == Some("\\cxds") {
            // A suffix
            piece = format!("{{{piece}^}}");
        } else {
            tokens.rewind(after);
        }
        current.text.push_str(&piece);
    }
    if let Some(outline) = steno {
        entries.push((outline, finalize_translation(&current.text)));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(rtf: &str) -> Vec<(String, String)> {
        parse_rtf(rtf.as_bytes()).unwrap()
    }

    fn pair(outline: &str, translation: &str) -> (String, String) {
        (outline.to_string(), translation.to_string())
    }

    #[test]
    fn plovers_example_dictionary() {
        let rtf = "{\\rtf1\\ansi\\cxrev100\\cxdict\n{\\*\\cxs KAT}cat\n{\\*\\cxs KAT/HROG}catalog\n\
                   {\\*\\cxs KA/TA/HROG}catalog\n{\\*\\cxs -S}\\cxds s{\\*\\cxcomment -s suffix}\n}";
        assert_eq!(
            entries(rtf),
            [
                pair("KAT", "cat"),
                pair("KAT/HROG", "catalog"),
                pair("KA/TA/HROG", "catalog"),
                pair("-S", "{^s}"),
            ]
        );
    }

    #[test]
    fn a_file_that_is_not_rtf_is_refused() {
        assert!(parse_rtf(b"{\"KAT\": \"cat\"}").is_err());
    }

    #[test]
    fn the_controls_become_the_translation_language() {
        let rtf = "{\\rtf1\\ansi{\\*\\cxrev100}\\cxdict\
            {\\*\\cxs A}\\cxfc hello\
            {\\*\\cxs B}\\cxfl World\
            {\\*\\cxs C}{\\cxp. }\
            {\\*\\cxs D}re\\cxds \
            {\\*\\cxs E}\\cxds x\\cxds \
            {\\*\\cxs F}\\cxds\
            {\\*\\cxs G}{\\cxfing q}\
            {\\*\\cxs H}\\cxdstroke\
            {\\*\\cxs I}a\\~b\
            }";
        assert_eq!(
            entries(rtf),
            [
                pair("A", "{-|}hello"),
                pair("B", "{>}World"),
                pair("C", "{.}"),
                pair("D", "{re^}"),
                pair("E", "{^x^}"),
                pair("F", "{^}"),
                pair("G", "{&q}"),
                pair("H", "=undo"),
                pair("I", "a{^ ^}b"),
            ]
        );
    }

    #[test]
    fn unicode_escapes_and_their_fallbacks() {
        let rtf = "{\\rtf1{\\*\\cxs A}caf\\u233?\\par x{\\*\\cxs B}{\\uc0\\u8364 }}";
        assert_eq!(
            entries(rtf),
            [pair("A", "caf\u{e9}\n\nx"), pair("B", "\u{20ac}")]
        );
    }

    fn from_hex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Plover's own answers for RTF files it writes from a dictionary of every operator, and for
    /// hand-written ones with the corners of the format. See `tools/plover-golden`.
    #[test]
    fn rtf_files_read_exactly_as_plover_reads_them() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("testdata/rtf_golden.json")).unwrap();
        let mut wrong = Vec::new();
        for case in &cases {
            let name = case["name"].as_str().unwrap();
            let bytes = from_hex(case["rtf"].as_str().unwrap());
            let got = parse_rtf(&bytes);
            if case["error"].as_bool() == Some(true) {
                if got.is_ok() {
                    wrong.push(format!("{name}: Plover refuses it, keymux read it"));
                }
                continue;
            }
            let want: Vec<(String, String)> = case["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    (
                        e[0].as_str().unwrap().to_string(),
                        e[1].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            match got {
                Ok(got) if got == want => {}
                Ok(got) => wrong.push(format!("{name}:\n    plover {want:?}\n    keymux {got:?}")),
                Err(e) => wrong.push(format!("{name}: Plover reads it, keymux refuses it: {e}")),
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} files differ from Plover:\n{}",
            wrong.len(),
            cases.len(),
            wrong.join("\n")
        );
    }

    #[test]
    fn windows_1252_is_decoded() {
        let mut rtf = b"{\\rtf1{\\*\\cxs A}".to_vec();
        rtf.extend_from_slice(&[0x93, b'q', 0x94, 0xe9]);
        rtf.push(b'}');
        assert_eq!(
            parse_rtf(&rtf).unwrap(),
            [pair("A", "\u{201c}q\u{201d}\u{e9}")]
        );
    }
}
