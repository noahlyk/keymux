//! Which dictionary files a steno layer reads, found by walking its folders.
//!
//! A layer's dictionaries are a list of files and folders. A folder contributes every `.json`
//! file in it, however deeply nested, so adding a dictionary means dropping a file in the
//! steno folder, with no config to edit.
//!
//! - **Priority.** Earlier wins when two entries share a stroke. A folder is one unit: it ranks
//!   among its neighbours by its own name, everything inside it stays together at that spot, and
//!   the order inside only decides between the things in it. Among the entries of a folder:
//!   1. Names that start with a number, files and folders alike, lowest number first (`2-`
//!      before `10-`). However big the number, a numbered name beats any plain one: a number
//!      is a priority, and having none is the lowest priority there is.
//!   2. Plain files, which have no number.
//!   3. Plain folders.
//!
//!   Within each of those, and between entries with the same number, the one that sorts first
//!   alphabetically (ignoring case) comes first, and the exact name (capitals first) settles
//!   what is left, so the order never depends on how the disk lists them.
//! - **Switching off.** A file or folder whose name starts with `disabled` is skipped, so
//!   renaming `lapwing/` to `disabled_lapwing/` turns it off without deleting it.
//! - Dictionaries are `.json` and `.rtf` (RTF/CRE) files, and the orthography word list
//!   (`american_english_words.txt`) is found the same way. Hidden files (`.name`) and anything
//!   else are skipped.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A file or folder whose name starts with this is left out, whatever the case.
pub const DISABLED_PREFIX: &str = "disabled";

/// How deep folders may nest. A limit, so a link back to a parent can't loop forever.
const MAX_DEPTH: usize = 32;

/// Whether `name` marks a file or folder as switched off.
#[must_use]
pub fn is_disabled(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.get(..DISABLED_PREFIX.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(DISABLED_PREFIX))
    })
}

/// The dictionary files `roots` stand for, in priority order, highest first.
///
/// A root that is a file is used as it is, since it was named on purpose. A root that is a
/// folder is walked. `skip` is left out wherever it turns up: the folder holding cached copies.
#[must_use]
pub fn expand(roots: &[PathBuf], skip: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        if root.is_dir() {
            walk(root, skip, 0, &mut files);
        } else if root.exists() {
            files.push(root.clone());
        }
    }
    // The same file listed twice keeps its first, higher, place
    let mut seen = std::collections::HashSet::new();
    files.retain(|file| seen.insert(file.clone()));
    files
}

fn walk(dir: &Path, skip: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let hidden = name.to_str().is_some_and(|name| name.starts_with('.'));
        if hidden || is_disabled(&name) || path == skip {
            continue;
        }
        // `metadata` follows links, so a linked file or folder counts as what it points to
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => entries.push((order_key(&name, true), path, true)),
            Ok(meta) if meta.is_file() && is_dictionary_file(&path) => {
                entries.push((order_key(&name, false), path, false));
            }
            _ => {}
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, path, is_folder) in entries {
        if !is_folder {
            files.push(path);
        } else if depth < MAX_DEPTH {
            // Everything in the folder goes here, before the next entry
            walk(&path, skip, depth + 1, files);
        }
    }
}

/// Where a file or folder ranks among the entries of its folder. Smaller is earlier: the group
/// (numbered, plain file, plain folder), its number, the rest of the name without case, and the
/// exact name, which settles anything the others leave tied.
type OrderKey = (u8, u64, String, String);

fn order_key(name: &std::ffi::OsStr, is_folder: bool) -> OrderKey {
    let name = name.to_string_lossy().into_owned();
    let digits = name.chars().take_while(char::is_ascii_digit).count();
    // Digits are a number only if they stand alone: `00-user` and `10.json` are numbered,
    // `3dprinting.json` is just a name that starts with a 3
    let alone = name[digits..]
        .chars()
        .next()
        .is_none_or(|next| !next.is_alphanumeric());
    if digits > 0 && alone {
        // A number too long to hold counts as the largest, still ahead of every plain name
        let number = name[..digits].parse().unwrap_or(u64::MAX);
        return (0, number, name[digits..].to_lowercase(), name);
    }
    (if is_folder { 2 } else { 1 }, 0, name.to_lowercase(), name)
}

/// Whether a file is read as part of the dictionary: JSON, RTF/CRE, or the word list.
fn is_dictionary_file(path: &Path) -> bool {
    let kind = |wanted: &str| {
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(wanted))
    };
    kind("json")
        || kind("rtf")
        || path.file_name().and_then(|name| name.to_str())
            == Some(super::orthography::WORDLIST_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh folder with these files in it (empty contents), and its path.
    fn tree(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keymux-source-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for file in files {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "{}").unwrap();
        }
        dir
    }

    fn names(dir: &Path, files: &[PathBuf]) -> Vec<String> {
        files
            .iter()
            .map(|file| file.strip_prefix(dir).unwrap().display().to_string())
            .collect()
    }

    #[test]
    fn plain_files_come_before_plain_folders_and_each_group_is_alphabetical() {
        let dir = tree(
            "order",
            &["b.json", "A.json", "lapwing/x.json", "zeta/y.json"],
        );
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        assert_eq!(
            names(&dir, &found),
            ["A.json", "b.json", "lapwing/x.json", "zeta/y.json"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_numbered_folder_ranks_against_files_by_its_own_number() {
        let dir = tree(
            "numbered",
            &[
                "50-late.json",
                "20-mine/a.json",
                "20-mine/00-first.json",
                "05-early.json",
                "plain.json",
                "lapwing/x.json",
            ],
        );
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        // The folder is one unit at its own number. What is inside only orders the inside.
        assert_eq!(
            names(&dir, &found),
            [
                "05-early.json",
                "20-mine/00-first.json",
                "20-mine/a.json",
                "50-late.json",
                "plain.json",
                "lapwing/x.json",
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbers_compare_as_numbers_not_as_text() {
        let dir = tree(
            "natural",
            &["10-b.json", "2-a.json", "100-c.json", "1-z.json"],
        );
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        assert_eq!(
            names(&dir, &found),
            ["1-z.json", "2-a.json", "10-b.json", "100-c.json"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_number_goes_alphabetically_files_and_folders_together() {
        let dir = tree(
            "same-number",
            &["10-b.json", "10-A.json", "010-c.json", "10/x.json"],
        );
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        // 10 and 010 are the same number, so the rest of the name decides, ignoring case. A
        // folder called just "10" has nothing after the number, so it is first.
        assert_eq!(
            names(&dir, &found),
            ["10/x.json", "10-A.json", "10-b.json", "010-c.json"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbered_come_first_whatever_the_number_then_plain_files_then_plain_folders() {
        let dir = tree(
            "groups",
            &[
                "plain.json",
                "zeta/y.json",
                "49-above.json",
                "10002-huge.json",
                "00-user.json",
                "99-folder/base.json",
                "100-after.json",
            ],
        );
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        // Even 10002 beats a plain name: no number is the lowest priority there is
        assert_eq!(
            names(&dir, &found),
            [
                "00-user.json",
                "49-above.json",
                "99-folder/base.json",
                "100-after.json",
                "10002-huge.json",
                "plain.json",
                "zeta/y.json",
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_only_numbered_folder_beats_everything_plain() {
        let dir = tree(
            "only-numbered",
            &["00-lapwing/base.json", "a.json", "b/c.json", "z.json"],
        );
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        assert_eq!(
            names(&dir, &found),
            ["00-lapwing/base.json", "a.json", "z.json", "b/c.json"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn digits_that_are_part_of_a_word_are_not_a_number() {
        let dir = tree(
            "word",
            &["3dprinting.json", "7.json", "a.json", "2024-notes.json"],
        );
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        // 7 and 2024 are numbers, 3dprinting is just a plain name
        assert_eq!(
            names(&dir, &found),
            ["7.json", "2024-notes.json", "3dprinting.json", "a.json"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_the_same_but_for_case_are_ordered_by_the_exact_name_not_by_the_disk() {
        let dir = tree("case", &["b.json", "B.json", "a.json", "A.json"]);
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        assert_eq!(
            names(&dir, &found),
            ["A.json", "a.json", "B.json", "b.json"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders_nest_as_deep_as_you_like() {
        let dir = tree("deep", &["a/b/c/d/e/f/g.json", "a/b/top.json"]);
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        assert_eq!(names(&dir, &found), ["a/b/top.json", "a/b/c/d/e/f/g.json"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn anything_starting_with_disabled_is_left_out_at_any_depth() {
        let dir = tree(
            "disabled",
            &[
                "keep.json",
                "disabled_old.json",
                "Disabled.json",
                "disabled_lapwing/base.json",
                "a/disabled-x/deep.json",
                "a/disabled.json",
                "a/keep.json",
            ],
        );
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        assert_eq!(names(&dir, &found), ["keep.json", "a/keep.json"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_dictionaries_and_the_word_list_and_not_hidden_ones_or_the_cache() {
        let dir = tree(
            "kinds",
            &[
                "a.json",
                "b.txt",
                "c.modal",
                ".hidden.json",
                "a.json.part",
                "cache/x.json",
                "d.RTF",
                "american_english_words.txt",
            ],
        );
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        assert_eq!(
            names(&dir, &found),
            ["a.json", "american_english_words.txt", "d.RTF"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_named_as_a_root_is_used_even_if_it_looks_disabled_and_a_missing_one_is_skipped() {
        let dir = tree("explicit", &["disabled_mine.json"]);
        let roots = [dir.join("disabled_mine.json"), dir.join("missing.json")];
        let found = expand(&roots, &dir.join("cache"));
        assert_eq!(names(&dir, &found), ["disabled_mine.json"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roots_keep_their_order_and_a_repeated_file_keeps_its_first_place() {
        let dir = tree("roots", &["one/a.json", "two/b.json"]);
        let roots = [dir.join("two"), dir.join("one"), dir.join("two/b.json")];
        let found = expand(&roots, &dir.join("cache"));
        assert_eq!(names(&dir, &found), ["two/b.json", "one/a.json"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_back_to_a_parent_folder_does_not_loop_forever() {
        let dir = tree("loop", &["a.json"]);
        std::os::unix::fs::symlink(&dir, dir.join("again")).unwrap();
        let found = expand(std::slice::from_ref(&dir), &dir.join("cache"));
        assert_eq!(found.first().map(|f| f.ends_with("a.json")), Some(true));
        assert!(found.len() <= MAX_DEPTH + 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
