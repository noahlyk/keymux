//! The `{...}` operators of Plover's translation language, as a port of Plover's meta plugins
//! (`plover/meta/*.py`) and the parser that picks which one a `{...}` is.
//!
//! `{^ing}` is the attach meta with `^ing`, `{-|}` the case meta with `cap_first_word`, and so on.
//! The parser tries the shapes in Plover's order, and the first that fits the whole text wins.

use super::action::{
    apply_case, has_word_boundary, is_space, rightmost_word, Action, Case, SPACE,
};
use super::formatter::Context;

const ATTACH: char = '^';
const CARRY: &str = "~|";

/// A meta recognised in `{...}`: its name and what follows it.
#[derive(Debug, PartialEq, Eq)]
pub struct Meta<'a> {
    pub name: String,
    pub param: &'a str,
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

/// Which meta `meta` (the text between the braces) is, and its argument. `None` when it is
/// nothing Plover knows, which does nothing.
#[must_use]
pub fn parse_meta(meta: &str) -> Option<Meta<'_>> {
    let found = |name: &str, param: &'static str| Some(Meta { name: name.to_string(), param });
    // Generic `{:name:argument}`
    if let Some(rest) = meta.strip_prefix(':') {
        let end = rest.find(':').unwrap_or(rest.len());
        if end > 0 {
            let after = &rest[end..];
            let param = after.strip_prefix(':').unwrap_or(after);
            return Some(Meta { name: rest[..end].to_string(), param });
        }
    }
    if let Some(command) = strip_prefix_ignore_case(meta, "PLOVER:") {
        return Some(Meta { name: "command".into(), param: command });
    }
    if let Some(combo) = meta.strip_prefix('#') {
        return Some(Meta { name: "key_combo".into(), param: combo });
    }
    match meta {
        "," | ":" | ";" => return Some(Meta { name: "comma".into(), param: meta }),
        "." | "!" | "?" => return Some(Meta { name: "stop".into(), param: meta }),
        "-|" => return found("case", "cap_first_word"),
        ">" => return found("case", "lower_first_char"),
        "<" => return found("case", "upper_first_word"),
        "*-|" => return found("retro_case", "cap_first_word"),
        "*>" => return found("retro_case", "lower_first_char"),
        "*<" => return found("retro_case", "upper_first_word"),
        "$" => return found("word_end", ""),
        _ => {}
    }
    if let Some(pattern) = meta.strip_prefix('=') {
        return Some(Meta { name: "if_next_matches".into(), param: pattern });
    }
    if let Some(mode) = strip_prefix_ignore_case(meta, "MODE:") {
        return Some(Meta { name: "mode".into(), param: mode });
    }
    if let Some(format) = meta.strip_prefix("*(").and_then(|rest| rest.strip_suffix(')')) {
        return Some(Meta { name: "retro_currency".into(), param: format });
    }
    if let Some(text) = meta.strip_prefix('&') {
        return Some(Meta { name: "glue".into(), param: text });
    }
    // `^?~|text^?`: carry the capitalization over the text
    let after_attach = meta.strip_prefix(ATTACH).unwrap_or(meta);
    if after_attach.starts_with(CARRY) {
        return Some(Meta { name: "carry_capitalize".into(), param: meta });
    }
    if meta.starts_with(ATTACH) || meta.ends_with(ATTACH) {
        return Some(Meta { name: "attach".into(), param: meta });
    }
    None
}

/// The action for `meta`, in the context of what came before. A meta that is unknown, or that
/// has an argument it can't use, does nothing.
#[must_use]
pub fn meta_to_action(meta: &str, ctx: &Context) -> Action {
    let Some(Meta { name, param }) = parse_meta(meta) else {
        return ctx.new_action();
    };
    match name.to_lowercase().as_str() {
        "attach" => meta_attach(ctx, param),
        "carry_capitalize" => meta_carry_capitalize(ctx, param),
        "case" => meta_case(ctx, param),
        "retro_case" => meta_retro_case(ctx, param),
        "comma" => meta_comma(ctx, param),
        "stop" => meta_stop(ctx, param),
        "command" => {
            let mut action = ctx.copy_last_action();
            action.command = Some(param.to_string());
            action
        }
        "glue" => meta_glue(ctx, param),
        "if_next_matches" => meta_if_next_matches(ctx, param),
        "key_combo" => {
            let mut action = ctx.copy_last_action();
            action.combo = Some(param.to_string());
            action
        }
        "mode" => meta_mode(ctx, param),
        "retro_currency" => meta_retro_currency(ctx, param),
        "word_end" => {
            let mut action = ctx.copy_last_action();
            action.word_is_finished = true;
            action
        }
        // keymux's own: `{:firstup}` marks a stroke the engine sends when its first key comes up.
        // It types nothing and changes nothing around it.
        "firstup" => ctx.copy_last_action(),
        // Lapwing's stitching: `{:stitch:A}` is a fingerspelled letter that joins with a dash
        "stitch" => meta_stitch(ctx, param),
        "stitch_last_word" => meta_stitch_last_word(ctx, param),
        _ => ctx.new_action(),
    }
}

/// `{:stitch:word}` or `{:stitch:word:delimiter}`: glued like fingerspelling, and after another
/// glued piece joined to it with the delimiter (a dash unless told otherwise).
fn meta_stitch(ctx: &Context, cmdline: &str) -> Action {
    let mut args = cmdline.split(':');
    let word = args.next().unwrap_or_default();
    let delimiter = args.next().unwrap_or("-");
    let mut action = ctx.new_action();
    action.prev_attach = ctx.last_action.glue || ctx.last_action.next_attach;
    action.glue = true;
    action.text = Some(if ctx.last_action.glue {
        format!("{delimiter}{word}")
    } else {
        word.to_string()
    });
    action
}

/// Join the characters of each word with `delimiter`: `cat` is `c-a-t`.
fn stitch_word(text: &str, delimiter: &str) -> String {
    let mut out = String::new();
    for word in super::action::words(text) {
        let bare = word.trim_end();
        let letters: Vec<String> = bare.chars().map(String::from).collect();
        out.push_str(&letters.join(delimiter));
        out.push_str(&word[bare.len()..]);
    }
    out
}

/// `{:stitch_last_word:count:delimiter}`: the last words rewritten with dashes between letters.
fn meta_stitch_last_word(ctx: &Context, cmdline: &str) -> Action {
    let mut args = cmdline.split(':');
    let count = args
        .next()
        .filter(|n| !n.is_empty())
        .map_or(1, |n| n.parse().unwrap_or(1));
    let delimiter = args.next().unwrap_or("-");
    let mut action = ctx.copy_last_action();
    let last = ctx.last_words(count).concat();
    if !last.is_empty() {
        action.text = Some(stitch_word(&last, delimiter));
        action.prev_replace = last;
        action.prev_attach = true;
        action.word = None;
    }
    action
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

fn meta_attach(ctx: &Context, meta: &str) -> Action {
    let mut action = ctx.new_action();
    let mut meta = meta.to_string();
    let mut begin = meta.starts_with(ATTACH);
    let mut end = meta.ends_with(ATTACH);
    if !(begin || end) {
        // If not specified, attach at both ends
        meta = format!("{ATTACH}{meta}{ATTACH}");
        begin = true;
        end = true;
    }
    if begin {
        meta.remove(0);
        action.prev_attach = true;
    }
    if end {
        meta.pop();
        action.next_attach = true;
        action.word_is_finished = false;
    }
    let mut last_word = ctx.last_action.word.clone().unwrap_or_default();
    if meta.is_empty() {
        // An empty connection is a break in the orthography rules: the writer says don't
        // correct the word
        action.orthography = false;
    } else if !last_word.is_empty()
        && !is_space(&meta)
        && ctx.last_action.orthography
        && begin
        && (!end || has_word_boundary(&meta))
    {
        let new_word = ctx.orthography.add_suffix(&last_word, &meta);
        let common = common_prefix_len(&last_word, &new_word);
        let replaced: String = last_word.chars().skip(common).collect();
        action.prev_replace = ctx.last_text(replaced.chars().count());
        last_word = last_word.chars().take(common).collect();
        meta = new_word.chars().skip(common).collect();
    }
    action.text = Some(meta.clone());
    if action.prev_attach {
        action.word = Some(rightmost_word(&format!("{last_word}{meta}")));
    }
    action
}

fn meta_carry_capitalize(ctx: &Context, meta: &str) -> Action {
    // Format: ^~|content^, the attach flags optional
    let mut action = ctx.new_action();
    if ctx.last_action.next_case == Some(Case::CapFirstWord) {
        action.next_case = Some(Case::CapFirstWord);
    }
    let mut meta = meta.to_string();
    let begin = meta.starts_with(ATTACH);
    if begin {
        meta.remove(0);
        action.prev_attach = true;
    }
    meta = meta[CARRY.len().min(meta.len())..].to_string();
    let end = meta.ends_with(ATTACH);
    if end {
        meta.pop();
        action.next_attach = true;
        action.word_is_finished = false;
    }
    if !meta.is_empty() || begin || end {
        action.text = Some(meta);
    }
    action
}

fn meta_case(ctx: &Context, case: &str) -> Action {
    let Some(case) = Case::parse(case) else {
        return ctx.new_action();
    };
    let mut action = ctx.copy_last_action();
    action.next_case = Some(case);
    action
}

fn meta_retro_case(ctx: &Context, case: &str) -> Action {
    let Some(case) = Case::parse(case) else {
        return ctx.new_action();
    };
    let mut action = ctx.copy_last_action();
    action.prev_attach = true;
    if let Some(word) = ctx.last_word() {
        action.text = Some(apply_case(&word, Some(case)));
        action.prev_replace = word;
    } else {
        action.text = Some(String::new());
    }
    action
}

fn meta_comma(ctx: &Context, text: &str) -> Action {
    let mut action = ctx.new_action();
    action.text = Some(text.to_string());
    action.prev_attach = true;
    action
}

fn meta_stop(ctx: &Context, text: &str) -> Action {
    let mut action = ctx.new_action();
    action.prev_attach = true;
    action.text = Some(text.to_string());
    action.next_case = Some(Case::CapFirstWord);
    action
}

fn meta_glue(ctx: &Context, text: &str) -> Action {
    let mut action = ctx.new_action();
    action.glue = true;
    action.text = Some(text.to_string());
    if ctx.last_action.glue {
        action.prev_attach = true;
    }
    action
}

/// Plover splits `pattern/yes/no` on slashes that aren't escaped with a backslash, and then
/// takes the escapes out (`\/` and `\\`).
fn split_if_next(meta: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = meta.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('\\' | '/')) => {
                // An escaped backslash or slash is just that character
                if let Some(escaped) = chars.next() {
                    if let Some(part) = parts.last_mut() {
                        part.push(escaped);
                    }
                }
            }
            '/' if parts.len() < 3 => parts.push(String::new()),
            other => {
                if let Some(part) = parts.last_mut() {
                    part.push(other);
                }
            }
        }
    }
    // Plover drops empty parts before looking at which is which
    parts.retain(|part| !part.is_empty());
    parts
}

fn meta_if_next_matches(ctx: &Context, meta: &str) -> Action {
    let parts = split_if_next(meta);
    let [pattern, yes, no] = parts.as_slice() else {
        return ctx.new_action();
    };
    let alternative = |text: &String| {
        let mut action = ctx.new_action();
        action.text = Some(text.clone());
        action
    };
    Action::look_ahead(pattern.clone(), alternative(yes), alternative(no))
}

fn meta_mode(ctx: &Context, cmdline: &str) -> Action {
    let (mode, arg) = cmdline
        .split_once(':')
        .map_or((cmdline, None), |(mode, arg)| (mode, Some(arg)));
    let mode = mode.to_lowercase();
    let mut action = ctx.copy_last_action();
    if mode == "set_space" {
        action.space_char = arg.unwrap_or("").to_string();
        return action;
    }
    // No argument is allowed for the other modes
    if arg.is_some() {
        return ctx.new_action();
    }
    match mode.as_str() {
        "caps" => action.case = Some(Case::Upper),
        "title" => action.case = Some(Case::Title),
        "lower" => action.case = Some(Case::Lower),
        "snake" => action.space_char = "_".to_string(),
        "camel" => {
            action.case = Some(Case::Title);
            action.space_char = String::new();
            action.next_case = Some(Case::LowerFirstChar);
        }
        "reset" => {
            action.space_char = SPACE.to_string();
            action.case = None;
        }
        "reset_space" => action.space_char = SPACE.to_string(),
        "reset_case" => action.case = None,
        _ => return ctx.new_action(),
    }
    action
}

/// A whole number written with a comma between every three digits, as Python's `{:,}`.
fn group_thousands(digits: &str) -> String {
    let mut out = String::new();
    let len = digits.chars().count();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn format_int(value: i128) -> String {
    let sign = if value < 0 { "-" } else { "" };
    format!("{sign}{}", group_thousands(&value.unsigned_abs().to_string()))
}

fn format_float(value: f64) -> String {
    // Python's `{:,.2f}`
    let text = format!("{:.2}", value.abs());
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, "00"));
    let sign = if value.is_sign_negative() && text != "0.00" { "-" } else { "" };
    format!("{sign}{}.{fraction}", group_thousands(whole))
}

fn meta_retro_currency(ctx: &Context, dict_format: &str) -> Action {
    let mut action = ctx.copy_last_action();
    let Some(word) = ctx.last_word() else {
        return action;
    };
    let currency = word.replace(',', "");
    let currency = currency.trim();
    let formatted = if let Ok(whole) = currency.parse::<i128>() {
        format_int(whole)
    } else if let Ok(float) = currency.parse::<f64>() {
        format_float(float)
    } else {
        return action;
    };
    action.prev_attach = true;
    action.prev_replace = word;
    action.text = Some(dict_format.replace('c', &formatted));
    action.word = None;
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_and_param(meta: &str) -> Option<(String, String)> {
        parse_meta(meta).map(|m| (m.name, m.param.to_string()))
    }

    #[test]
    fn the_operators_of_the_translation_language_are_told_apart() {
        for (meta, name, param) in [
            ("^", "attach", "^"),
            ("^ing", "attach", "^ing"),
            ("re^", "attach", "re^"),
            ("^-to-^", "attach", "^-to-^"),
            (":attach", "attach", ""),
            (":attach:^ing", "attach", "^ing"),
            (":attach: ", "attach", " "),
            (":attach:", "attach", ""),
            ("-|", "case", "cap_first_word"),
            (":case:upper_first_word", "case", "upper_first_word"),
            ("*>", "retro_case", "lower_first_char"),
            (".", "stop", "."),
            (",", "comma", ","),
            (":", "comma", ":"),
            (":comma::", "comma", ":"),
            ("&a", "glue", "a"),
        ] {
            let got = name_and_param(meta);
            assert_eq!(got, Some((name.to_string(), param.to_string())), "{meta}");
        }
        assert_eq!(name_and_param("#Return"), Some(("key_combo".into(), "Return".into())));
        assert_eq!(name_and_param("PLOVER:TOGGLE"), Some(("command".into(), "TOGGLE".into())));
        assert_eq!(name_and_param("plover:lookup"), Some(("command".into(), "lookup".into())));
        assert_eq!(name_and_param("mode:caps"), Some(("mode".into(), "caps".into())));
        assert_eq!(name_and_param("*($c)"), Some(("retro_currency".into(), "$c".into())));
        assert_eq!(name_and_param("=a/b/c"), Some(("if_next_matches".into(), "a/b/c".into())));
        assert_eq!(name_and_param("~|\"^"), Some(("carry_capitalize".into(), "~|\"^".into())));
        assert_eq!(name_and_param("^~|\""), Some(("carry_capitalize".into(), "^~|\"".into())));
        assert_eq!(name_and_param("$"), Some(("word_end".into(), String::new())));
        assert_eq!(name_and_param("nothing"), None);
    }

    #[test]
    fn if_next_splits_on_unescaped_slashes() {
        assert_eq!(split_if_next("[aeiou]/an/a"), ["[aeiou]", "an", "a"]);
        assert_eq!(split_if_next(r"a\/b/slash/none"), ["a/b", "slash", "none"]);
        assert_eq!(split_if_next(r"\\d/#/_"), [r"\d", "#", "_"]);
    }

    #[test]
    fn currency_is_grouped_like_python() {
        assert_eq!(format_int(1_234_567), "1,234,567");
        assert_eq!(format_int(-1234), "-1,234");
        assert_eq!(format_int(12), "12");
        assert_eq!(format_float(1234.5), "1,234.50");
        assert_eq!(format_float(0.456), "0.46");
        assert_eq!(format_float(-0.001), "0.00");
    }
}
