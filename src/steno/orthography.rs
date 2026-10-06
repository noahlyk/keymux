//! English spelling rules for attaching a suffix to a word.
//!
//! `make` + `ing` is `making`, and `carry` + `s` is `carries`. This covers the
//! common cases that Plover's suffix entries rely on. It is not the full Plover
//! rule table.

/// The word `stem` with `suffix` attached, spelled the way English would.
#[must_use]
pub fn attach(stem: &str, suffix: &str) -> String {
    if stem.is_empty() {
        return suffix.to_string();
    }
    if suffix.is_empty() {
        return stem.to_string();
    }
    let lower = stem.to_ascii_lowercase();
    let suf = suffix.to_ascii_lowercase();
    let starts_vowel = suf.starts_with(['a', 'e', 'i', 'o', 'u']);

    // -s / -es
    if suf == "s" {
        if let Some(stripped) = lower.strip_suffix('y').filter(|_| ends_consonant_y(&lower)) {
            return format!("{}{}", &stem[..stripped.len()], "ies");
        }
        if ends_with_any(&lower, &["s", "x", "z", "ch", "sh"]) {
            return format!("{stem}es");
        }
        return format!("{stem}s");
    }

    // ie + i-suffix: lie + ing = lying
    if lower.ends_with("ie") && suf.starts_with('i') {
        return format!("{}y{suffix}", &stem[..stem.len() - 2]);
    }

    // consonant + y + e-suffix: carry + ed = carried
    if ends_consonant_y(&lower) && suf.starts_with('e') {
        return format!("{}i{suffix}", &stem[..stem.len() - 1]);
    }

    // silent e before a vowel suffix: make + ing = making, make + ed = made
    if lower.ends_with('e') && !ends_with_any(&lower, &["ee", "ye", "oe"]) {
        if suf.starts_with('e') {
            return format!("{stem}{}", &suffix[1..]);
        }
        if starts_vowel {
            return format!("{}{suffix}", &stem[..stem.len() - 1]);
        }
    }

    // ee + e-suffix: agree + ed = agreed
    if lower.ends_with("ee") && suf.starts_with('e') {
        return format!("{stem}{}", &suffix[1..]);
    }

    // Double the final consonant of a one-syllable word: stop + ing = stopping
    if starts_vowel && is_doubling_stem(&lower) {
        let last = stem.chars().last().unwrap_or_default();
        return format!("{stem}{last}{suffix}");
    }

    format!("{stem}{suffix}")
}

fn ends_with_any(word: &str, endings: &[&str]) -> bool {
    endings.iter().any(|ending| word.ends_with(ending))
}

const fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// A `y` that follows a consonant. `day` ends in a vowel + y and keeps the y.
fn ends_consonant_y(word: &str) -> bool {
    let mut chars = word.chars().rev();
    chars.next() == Some('y') && chars.next().is_some_and(|c| !is_vowel(c))
}

/// A one-syllable word ending consonant-vowel-consonant, where the last
/// consonant is not w, x, or y. Those double before a vowel suffix.
fn is_doubling_stem(word: &str) -> bool {
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    if n < 3 || !chars.iter().all(char::is_ascii_alphabetic) {
        return false;
    }
    let last = chars[n - 1];
    let before_last = chars[n - 2];
    let consonant_end =
        !is_vowel(last) && !matches!(last, 'w' | 'x' | 'y') && is_vowel(before_last);
    consonant_end && vowel_groups(&chars) == 1
}

fn vowel_groups(chars: &[char]) -> usize {
    let mut groups = 0;
    let mut in_group = false;
    for &c in chars {
        if is_vowel(c) {
            if !in_group {
                groups += 1;
            }
            in_group = true;
        } else {
            in_group = false;
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_silent_e_before_vowel_suffix() {
        assert_eq!(attach("make", "ing"), "making");
        assert_eq!(attach("use", "able"), "usable");
    }

    #[test]
    fn e_suffix_after_silent_e_drops_its_e() {
        assert_eq!(attach("hope", "ed"), "hoped");
        assert_eq!(attach("hope", "er"), "hoper");
    }

    #[test]
    fn ee_keeps_its_e() {
        assert_eq!(attach("agree", "ed"), "agreed");
        assert_eq!(attach("agree", "ing"), "agreeing");
    }

    #[test]
    fn consonant_y_becomes_ies_and_ied() {
        assert_eq!(attach("carry", "s"), "carries");
        assert_eq!(attach("carry", "ed"), "carried");
        assert_eq!(attach("play", "s"), "plays");
        assert_eq!(attach("play", "ed"), "played");
    }

    #[test]
    fn sibilants_take_es() {
        assert_eq!(attach("box", "s"), "boxes");
        assert_eq!(attach("wish", "s"), "wishes");
        assert_eq!(attach("cat", "s"), "cats");
    }

    #[test]
    fn doubles_one_syllable_consonant_ending() {
        assert_eq!(attach("stop", "ing"), "stopping");
        assert_eq!(attach("run", "ing"), "running");
        assert_eq!(attach("fix", "ing"), "fixing");
        assert_eq!(attach("play", "ing"), "playing");
    }

    #[test]
    fn does_not_double_longer_words() {
        assert_eq!(attach("visit", "ing"), "visiting");
        assert_eq!(attach("open", "ed"), "opened");
    }

    #[test]
    fn ie_becomes_y_before_i() {
        assert_eq!(attach("lie", "ing"), "lying");
    }

    #[test]
    fn plain_attach_when_no_rule_applies() {
        assert_eq!(attach("cat", "s"), "cats");
        assert_eq!(attach("jump", "ed"), "jumped");
        assert_eq!(attach("", "ing"), "ing");
    }
}
