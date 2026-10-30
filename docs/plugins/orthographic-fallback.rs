//! An example keymux plugin: a phonetic best guess for a stroke that missed every dictionary.
//!
//! This is one self-contained `.rs` file, ordinary Rust, nothing but `std`. Drop it into your
//! steno folder (`~/.config/keymux/steno/`, any name ending in `.rs`), numbered after your
//! dictionaries so it only answers what they miss, and keymux compiles and loads it the next
//! time it starts, or the moment the file changes while it's running. See
//! `docs/plugins/README.md` for how the interface it implements works, and
//! `src/steno/plugin.rs` for how keymux calls it.
//!
//! It reads a stroke's own keys back into a plain, readable spelling: not real English
//! spelling, since the stroke alone can't say how a word is actually spelled, just close
//! enough to be read. `KAT` on its own (no dictionary entry for it) would read as `kat`;
//! `TKPWEUPB` would read as `gin`.

/// Clusters of left-hand keys, longest first, read from Plover's own theory: `TK` is one sound
/// (`d`), not `t` then `k`.
const ONSETS: &[(&str, &str)] = &[
    ("TKPW", "g"),
    ("SKWR", "j"),
    ("STKPW", "z"),
    ("TPH", "n"),
    ("KWR", "y"),
    ("TK", "d"),
    ("TP", "f"),
    ("PW", "b"),
    ("PH", "m"),
    ("SR", "v"),
    ("HR", "l"),
    ("KH", "ch"),
    ("SH", "sh"),
    ("TH", "th"),
    ("S", "s"),
    ("T", "t"),
    ("K", "k"),
    ("P", "p"),
    ("W", "w"),
    ("H", "h"),
    ("R", "r"),
];

/// Clusters of right-hand keys, longest first.
const CODAS: &[(&str, &str)] = &[
    ("PBLG", "j"),
    ("PBG", "ng"),
    ("FRPB", "nch"),
    ("FP", "ch"),
    ("RB", "sh"),
    ("BG", "k"),
    ("PB", "n"),
    ("PL", "m"),
    ("F", "f"),
    ("R", "r"),
    ("P", "p"),
    ("B", "b"),
    ("L", "l"),
    ("G", "g"),
    ("T", "t"),
    ("S", "s"),
    ("D", "d"),
    ("Z", "z"),
];

/// Vowel-key combinations, longest first: diphthongs and long vowels before single letters.
const VOWELS: &[(&str, &str)] = &[
    ("AOEU", "i"),
    ("OEU", "oy"),
    ("AEU", "ay"),
    ("AOE", "ee"),
    ("AOU", "oo"),
    ("OU", "ow"),
    ("AU", "aw"),
    ("OE", "oh"),
    ("EU", "i"),
    ("AO", "oo"),
    ("A", "a"),
    ("O", "o"),
    ("E", "e"),
    ("U", "u"),
    ("*", ""),
    // No vowel key down is a bare consonant skeleton, valid on its own (the start of a word a
    // join-key continuation finishes) or as all there really is: either way, not a cue to
    // invent a specific vowel sound nobody asked for.
    ("", ""),
];

/// Greedily reads `letters` as a sequence of the given clusters, longest match first, until
/// nothing is left. Unlike a real phonetic rule table, this never fails: a letter with no
/// cluster of its own just reads as itself, lower-cased.
fn spell(mut letters: &str, clusters: &[(&str, &str)]) -> String {
    let mut out = String::new();
    while !letters.is_empty() {
        let hit = clusters.iter().find(|(pattern, _)| {
            !pattern.is_empty() && letters.starts_with(pattern)
        });
        match hit {
            Some((pattern, sound)) => {
                out.push_str(sound);
                letters = &letters[pattern.len()..];
            }
            None => {
                out.push(letters.chars().next().unwrap().to_ascii_lowercase());
                letters = &letters[1..];
            }
        }
    }
    out
}

/// A stroke's keys split into its left-hand, vowel and right-hand letters. Simpler than
/// keymux's own parser (`src/steno/layout.rs`): good enough for a best guess, not for
/// validating that a stroke is well-formed.
fn parts(stroke: &str) -> (String, String, String) {
    let mut left = String::new();
    let mut vowel = String::new();
    let mut right = String::new();
    let mut past_vowel = false;
    for c in stroke.chars() {
        match c {
            '#' | '+' => {}
            '-' => past_vowel = true,
            'A' | 'O' | '*' | 'E' | 'U' => {
                vowel.push(c);
                past_vowel = true;
            }
            'S' | 'T' | 'K' | 'P' | 'W' | 'H' | 'R' if !past_vowel => left.push(c),
            other => right.push(other),
        }
    }
    (left, vowel, right)
}

/// This plugin's actual job: a stroke outline, in Plover notation (one stroke, or several
/// separated by `/`), read back as a plain spelling, one syllable per stroke.
fn read(outline: &str) -> String {
    outline
        .split('/')
        .map(|stroke| {
            let (left, vowel, right) = parts(stroke.trim_start_matches('+'));
            let vowel_sound = VOWELS
                .iter()
                .find(|(pattern, _)| *pattern == vowel)
                .map_or("", |(_, sound)| sound);
            format!("{}{vowel_sound}{}", spell(&left, ONSETS), spell(&right, CODAS))
        })
        .collect::<Vec<_>>()
        .join("")
}

// --- The interface keymux's plugin loader calls. See docs/plugins/README.md. ---

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut buf: Vec<u8> = vec![0u8; len.max(0) as usize];
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr as i32
}

#[no_mangle]
pub extern "C" fn fallback(ptr: i32, len: i32) -> i64 {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len.max(0) as usize) };
    let Ok(outline) = std::str::from_utf8(bytes) else {
        return -1;
    };
    let mut out = read(outline).into_bytes();
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}

/// Optional: whether a stroke continues the word before it, rather than starting one of its
/// own. keymux itself has no built-in notion of a "join key" - every stroke key, standard or a
/// theory's own extra one, is just a bit to the engine - so a theory that wants one answers
/// here. This example assumes its theory's own first extra key (bit 23, the one right after
/// keymux's standard 23) is the join key; adjust the bit if your theory's isn't, or if it has
/// more than one extra key and a different one is the one that means "continue".
#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 {
    i32::from(bits & (1 << 23) != 0)
}
