//! Checking dictionaries without running them: `keymux steno validate`, `keymux validate`, and the
//! warnings the daemon raises when a changed file is wrong.
//!
//! A dictionary that does not load is kept out silently (the previous one stays in use), and an
//! entry the engine cannot use is read as a no-op, so a typo in a `{:meta}` types nothing and
//! reports nothing. This reads the same files, in the same order, with the same loader code and
//! the layout's own stroke parser (see [`super::dict`]), and says what is wrong:
//!
//! | kind | severity | what |
//! |---|---|---|
//! | `unreadable_file` | error | the file cannot be read |
//! | `unparsable_file` | error | not a JSON object (or RTF/CRE dictionary); line and column when known |
//! | `bad_outline` | error | an outline key the layout does not parse |
//! | `bad_value` | error | an entry whose value is not a string |
//! | `unbalanced_braces` | error | an unclosed, stray or nested `{` `}` in an entry |
//! | `unknown_meta` | warning | a `{...}` that looks like a command but is none: the engine does nothing |
//! | `bad_meta_argument` | warning | a known meta with an argument it cannot use, so it does nothing (or less) |
//! | `misplaced_meta` | warning | a macro (`{:undo}`) that only works as an entry's whole text |
//! | `unknown_command` | warning | `{:run:NAME}` with a name the config's `commands` doesn't define (only checked with a config) |
//! | `untrusted_shell` | warning | an inline shell command in a file that is not trusted: ignored |
//! | `shell_command` | info | an inline shell command in a trusted file: it runs as the user |
//! | `legacy_alias` | info | a legacy `{PLOVER:...}` spelling of a native command; once per file, with a count |
//! | `legacy_no_effect` | info | a legacy command keymux has no counterpart for: it does nothing; once per file, with a count |
//! | `duplicate_outline` | warning | the same outline twice in one file; the last wins |
//! | `unreachable_outline` | warning | a multi-stroke outline holding a transparent stroke, which never enters the history |
//! | `shadowed_outline` | info | outlines also defined in an earlier file, which wins (the normal override) |
//!
//! The metas and verbs it knows come from [`super::meta::KNOWN`], the table the formatter runs,
//! and the macros from [`super::translate::macro_name`], so a name added there is accepted here.
//! Nothing is read twice per entry and nothing is compared pairwise: outlines are hashed once
//! (a 64-bit hash, so a cross-file duplicate is as good as certain, not proved), and one
//! finding per kind per file is capped at [`CAP`] with a count of the rest.

// The metas it talks about are written `{:name}`, markers and not format strings
#![allow(clippy::literal_string_with_formatting_args)]

use super::dict::{decode_text, read_entries};
use super::layout::Layout;
use super::meta::{self, KNOWN};
use super::orthography::WORDLIST_FILE;
use super::run_control::{self, Trust};
use super::source::{expand, is_plugin_file};
use super::translate::{is_transparent_mapping, macro_name, mapping_to_macro};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// The most findings of one kind reported for one file; the rest are counted in a last finding.
pub const CAP: usize = 100;

/// How serious a finding is. Errors are what make a file or entry unusable; warnings are
/// something the engine takes as a no-op or ignores; info is for what is probably intended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    /// The word for it, as the JSON writes it.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// What a finding is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    UnreadableFile,
    UnparsableFile,
    BadOutline,
    BadValue,
    UnbalancedBraces,
    UnknownMeta,
    BadMetaArgument,
    MisplacedMeta,
    UnknownCommand,
    UntrustedShell,
    ShellCommand,
    LegacyAlias,
    LegacyNoEffect,
    DuplicateOutline,
    UnreachableOutline,
    ShadowedOutline,
}

impl Kind {
    /// The name as the JSON writes it.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnreadableFile => "unreadable_file",
            Self::UnparsableFile => "unparsable_file",
            Self::BadOutline => "bad_outline",
            Self::BadValue => "bad_value",
            Self::UnbalancedBraces => "unbalanced_braces",
            Self::UnknownMeta => "unknown_meta",
            Self::BadMetaArgument => "bad_meta_argument",
            Self::MisplacedMeta => "misplaced_meta",
            Self::UnknownCommand => "unknown_command",
            Self::UntrustedShell => "untrusted_shell",
            Self::ShellCommand => "shell_command",
            Self::LegacyAlias => "legacy_alias",
            Self::LegacyNoEffect => "legacy_no_effect",
            Self::DuplicateOutline => "duplicate_outline",
            Self::UnreachableOutline => "unreachable_outline",
            Self::ShadowedOutline => "shadowed_outline",
        }
    }
}

/// One thing wrong (or noteworthy) in a dictionary file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// The file, as a path
    pub file: String,
    /// The entry's outline as the file writes it; `None` for a finding about the whole file
    pub outline: Option<String>,
    pub severity: Severity,
    pub kind: Kind,
    pub message: String,
    /// What to write instead, when there is a likely candidate
    pub suggestion: Option<String>,
    /// Where the entry is in a JSON file (the key's opening quote, 1-based), or where a file
    /// fails to parse, when known
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// How many entries this finding stands for, when it stands for several
    pub count: Option<usize>,
    /// The steno layers whose dictionaries include the file (empty when files were given by hand)
    pub layers: Vec<String>,
}

impl Finding {
    fn new(file: &Path, severity: Severity, kind: Kind, message: String) -> Self {
        Self {
            file: file.display().to_string(),
            outline: None,
            severity,
            kind,
            message,
            suggestion: None,
            line: None,
            column: None,
            count: None,
            layers: Vec::new(),
        }
    }
}

/// A steno layer's dictionaries: what the daemon loads for it.
#[derive(Debug, Clone)]
pub struct Target {
    /// The layers that share these files and layout (a keyboard's own layer is named
    /// `steno (keyboard "id")`)
    pub layers: Vec<String>,
    /// Files and folders, highest priority first, as the layer lists them
    pub roots: Vec<PathBuf>,
    /// The layer's stroke notation, extra keys and join keys included
    pub layout: Layout,
    /// What the config says about running commands
    pub rules: Rules,
}

/// What the config says about commands, for checking entries that run them.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    /// The files trusted to run inline shell commands
    pub trust: Trust,
    /// The config's `commands`, when a config is loaded; `{:run:NAME}` is only checked against it then
    pub commands: Option<HashMap<String, String>>,
}

impl Rules {
    /// What `config` says, for the steno folder in `config_dir`.
    #[must_use]
    pub fn of_config(config: &crate::config::Config, config_dir: &Path) -> Self {
        Self {
            trust: Trust::of_config(config, config_dir),
            commands: Some(config.commands.clone()),
        }
    }

    /// With no config: the standard trust, and no names to check against.
    #[must_use]
    pub fn standard(config_dir: &Path) -> Self {
        Self {
            trust: Trust::standard(&super::setup::steno_dir(config_dir)),
            commands: None,
        }
    }
}

/// Where an entry is, for the checks that depend on it.
struct Where<'a> {
    rules: &'a Rules,
    /// Whether its file is trusted to run inline shell commands
    trusted: bool,
}

/// The dictionaries of every steno layer in `config`, as the daemon builds them.
///
/// A layer with no `dictionaries` reads the whole steno folder. Layers with the same files and
/// layout are one target. A layer whose keys don't build a layout is `keymux validate`'s to
/// report and is left out here.
#[must_use]
pub fn targets_from_config(
    config: &crate::config::Config,
    config_dir: &Path,
    home: Option<&Path>,
) -> Vec<Target> {
    use crate::config::LayerKind;
    let own = config
        .per_keyboard_overrides
        .iter()
        .filter_map(|(id, keyboard)| {
            keyboard
                .layers
                .as_ref()
                .map(|layers| (Some(id.as_str()), layers))
        });
    let mut targets: Vec<Target> = Vec::new();
    let mut groups: Vec<(Option<&str>, &str, &crate::config::StenoConfig)> = Vec::new();
    for (keyboard, layers) in std::iter::once((None, &config.layers)).chain(own) {
        for (layer, layer_config) in layers {
            if let LayerKind::Steno(steno) = &layer_config.kind {
                groups.push((keyboard, layer.0.as_str(), steno));
            }
        }
    }
    groups.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    for (keyboard, layer, steno) in groups {
        let Ok(layout) = Layout::plover_with_joins(&steno.extra_keys, &steno.join_keys) else {
            continue;
        };
        let roots: Vec<PathBuf> = if steno.dictionaries.is_empty() {
            super::setup::default_dictionaries(config_dir)
        } else {
            steno
                .dictionaries
                .iter()
                .map(|raw| super::engine::resolve_dictionary_path(raw, config_dir, home))
                .collect()
        };
        let name = keyboard.map_or_else(
            || layer.to_string(),
            |id| format!("{layer} (keyboard \"{id}\")"),
        );
        match targets
            .iter_mut()
            .find(|target| target.roots == roots && target.layout == layout)
        {
            Some(target) => target.layers.push(name),
            None => targets.push(Target {
                layers: vec![name],
                roots,
                layout,
                rules: Rules::of_config(config, config_dir),
            }),
        }
    }
    targets
}

/// What validating the files of one target found.
#[derive(Debug, Clone, Serialize)]
pub struct Set {
    pub layers: Vec<String>,
    /// The dictionary files read, in priority order
    pub files: Vec<String>,
}

/// Everything a validation found.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    /// The sets of files read, one per distinct layer dictionary
    pub sets: Vec<Set>,
    /// How many different files were read
    pub files: usize,
    /// How many entries had a valid outline and a string value
    pub entries: usize,
    pub findings: Vec<Finding>,
}

impl Report {
    /// How many findings have this severity.
    #[must_use]
    pub fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == severity)
            .count()
    }

    /// Whether the exit code is success: no errors, and with `strict` no warnings either.
    #[must_use]
    pub fn passes(&self, strict: bool) -> bool {
        self.count(Severity::Error) == 0 && !(strict && self.count(Severity::Warning) > 0)
    }
}

/// Validate every target's files. `skip` is the folder holding cached copies, left out of any
/// folder walk exactly as the daemon leaves it out.
#[must_use]
pub fn validate(targets: &[Target], skip: &Path) -> Report {
    let mut report = Report::default();
    let mut seen_files: FxHashSet<PathBuf> = FxHashSet::default();
    let mut merged: HashMap<(String, Kind, Option<String>, String), usize> = HashMap::new();
    for target in targets {
        let mut files = expand(&target.roots, skip);
        files.retain(|file| is_dictionary(file));
        let (findings, entries) = validate_files_with(&files, &target.layout, &target.rules);
        // The same file under two layouts is checked twice, and reported once with both layers
        for mut finding in findings {
            finding.layers.clone_from(&target.layers);
            let key = (
                finding.file.clone(),
                finding.kind,
                finding.outline.clone(),
                finding.message.clone(),
            );
            if let Some(&at) = merged.get(&key) {
                for layer in finding.layers {
                    if !report.findings[at].layers.contains(&layer) {
                        report.findings[at].layers.push(layer);
                    }
                }
            } else {
                merged.insert(key, report.findings.len());
                report.findings.push(finding);
            }
        }
        report.entries = report.entries.max(entries);
        for file in &files {
            seen_files.insert(file.clone());
        }
        report.sets.push(Set {
            layers: target.layers.clone(),
            files: files
                .iter()
                .map(|file| file.display().to_string())
                .collect(),
        });
    }
    report.files = seen_files.len();
    // Grouped by file in load order, the worst first within a file
    let order: HashMap<&str, usize> = report
        .sets
        .iter()
        .flat_map(|set| set.files.iter())
        .enumerate()
        .map(|(at, file)| (file.as_str(), at))
        .collect();
    let mut findings = std::mem::take(&mut report.findings);
    findings.sort_by_key(|finding| {
        (
            order
                .get(finding.file.as_str())
                .copied()
                .unwrap_or(usize::MAX),
            std::cmp::Reverse(finding.severity),
        )
    });
    drop(order);
    report.findings = findings;
    report
}

/// What the cross-file checks keep while the files go by.
#[derive(Default)]
struct Cross {
    /// Strokes whose winning single-stroke entry is transparent
    transparent: FxHashSet<u32>,
    /// Outline hash to the first file that has it
    first: FxHashMap<u64, u32>,
    /// (the shadowed file, the file that wins) to how many outlines, and one of them
    shadows: FxHashMap<(u32, u32), (usize, String)>,
}

/// Validate one ordered set of files (highest priority first) against `layout`. The files are
/// read one at a time, so memory is the largest file plus a hash per distinct outline.
///
/// Returns the findings and how many entries were valid.
#[must_use]
pub fn validate_files(files: &[PathBuf], layout: &Layout) -> (Vec<Finding>, usize) {
    validate_files_with(files, layout, &Rules::default())
}

/// Like [`validate_files`], knowing what the config says about commands.
#[must_use]
pub fn validate_files_with(
    files: &[PathBuf],
    layout: &Layout,
    rules: &Rules,
) -> (Vec<Finding>, usize) {
    let dictionaries: Vec<&PathBuf> = files.iter().filter(|file| is_dictionary(file)).collect();
    let mut cross = Cross::default();
    // Pass one: which single strokes end up transparent. A stroke is what its first file says,
    // so every file is read to its single-stroke keys before any multi-stroke outline is judged.
    let mut winners: FxHashMap<u32, bool> = FxHashMap::default();
    for file in &dictionaries {
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        let keys = spelled_keys(file, layout);
        let _ = scan(file, &bytes, |key, value| {
            if key.contains('/') {
                return;
            }
            let stroke = keys.as_ref().map_or_else(
                || layout.parse_stroke(key),
                |notation| notation.parse_stroke(key),
            );
            if let Some(stroke) = stroke {
                let transparent =
                    value.is_some_and(is_transparent_mapping) && stroke & layout.join_mask() == 0;
                winners.entry(stroke).or_insert(transparent);
            }
        });
    }
    cross.transparent.extend(
        winners
            .iter()
            .filter(|(_, transparent)| **transparent)
            .map(|(stroke, _)| *stroke),
    );
    drop(winners);

    let mut findings = Vec::new();
    let mut entries = 0;
    for (index, file) in dictionaries.iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        entries += check_file_into(
            file,
            layout,
            rules,
            Some((&mut cross, index)),
            &mut findings,
        );
    }
    let mut shadows: Vec<_> = cross.shadows.into_iter().collect();
    shadows.sort_by_key(|((shadowed, winner), _)| (*shadowed, *winner));
    for ((shadowed, winner), (count, example)) in shadows {
        let (Some(file), Some(winner)) = (
            dictionaries.get(shadowed as usize),
            dictionaries.get(winner as usize),
        ) else {
            continue;
        };
        let mut finding = Finding::new(
            file,
            Severity::Info,
            Kind::ShadowedOutline,
            format!(
                "{count} outline{} also defined in {}, which loads earlier and wins",
                if count == 1 { " is" } else { "s are" },
                winner.display()
            ),
        );
        finding.outline = Some(example);
        finding.count = Some(count);
        if !is_rtf(file) {
            if let Ok(bytes) = std::fs::read(file) {
                locate(&decode_text(&bytes), std::slice::from_mut(&mut finding));
            }
        }
        findings.push(finding);
    }
    (findings, entries)
}

/// The findings of one file alone: what a reload of just that file can say, without comparing
/// it to the others. Used by the daemon when a file changes.
#[must_use]
pub fn check_file(file: &Path, layout: &Layout, rules: &Rules) -> Vec<Finding> {
    let mut findings = Vec::new();
    check_file_into(file, layout, rules, None, &mut findings);
    findings
}

fn is_dictionary(file: &Path) -> bool {
    !is_plugin_file(file) && file.file_name().and_then(|name| name.to_str()) != Some(WORDLIST_FILE)
}

fn is_rtf(file: &Path) -> bool {
    file.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("rtf"))
}

/// Fill in `line` and `column` of the findings that name an outline, from the JSON text of their
/// file: the first place the outline is written as a key (a string followed by a colon,
/// escapes decoded). The streaming reader keeps no positions, so this is a second, cheap pass
/// that only files with findings pay for, and it stops as soon as every outline is found.
fn locate(text: &str, findings: &mut [Finding]) {
    let mut wanted: HashMap<String, Option<(usize, usize)>> = findings
        .iter()
        .filter(|finding| finding.line.is_none())
        .filter_map(|finding| finding.outline.clone())
        .map(|outline| (outline, None))
        .collect();
    let mut remaining = wanted.len();
    let bytes = text.as_bytes();
    let (mut at, mut line, mut line_start) = (0, 1, 0);
    while at < bytes.len() && remaining > 0 {
        match bytes[at] {
            b'\n' => {
                line += 1;
                line_start = at + 1;
                at += 1;
            }
            b'"' => {
                let start = at;
                let (start_line, start_line_start) = (line, line_start);
                at += 1;
                let mut escaped = false;
                while at < bytes.len() && bytes[at] != b'"' {
                    match bytes[at] {
                        b'\\' => {
                            escaped = true;
                            at += 1;
                        }
                        b'\n' => {
                            line += 1;
                            line_start = at + 1;
                        }
                        _ => {}
                    }
                    at += 1;
                }
                if at >= bytes.len() {
                    break;
                }
                at += 1;
                let after = bytes[at..].iter().position(|b| !b.is_ascii_whitespace());
                if after.is_none_or(|skip| bytes[at + skip] != b':') {
                    continue;
                }
                let raw = &text[start..at];
                let key = if escaped {
                    serde_json::from_str::<String>(raw).ok()
                } else {
                    Some(raw[1..raw.len() - 1].to_string())
                };
                if let Some(slot) = key.and_then(|key| wanted.get_mut(&key)) {
                    if slot.is_none() {
                        let column = text[start_line_start..start].chars().count() + 1;
                        *slot = Some((start_line, column));
                        remaining -= 1;
                    }
                }
            }
            _ => at += 1,
        }
    }
    for finding in findings.iter_mut().filter(|finding| finding.line.is_none()) {
        let place = finding
            .outline
            .as_ref()
            .and_then(|outline| wanted.get(outline).copied().flatten());
        if let Some((line, column)) = place {
            finding.line = Some(line);
            finding.column = Some(column);
        }
    }
}

/// Every entry of a dictionary file as (outline text, value if it is a string), in order. JSON,
/// or RTF/CRE for `.rtf`; an error says why the file is no dictionary.
fn scan(
    file: &Path,
    bytes: &[u8],
    mut each: impl FnMut(&str, Option<&str>),
) -> Result<(), ScanError> {
    if file
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("rtf"))
    {
        let entries = super::rtf::parse_rtf(bytes).map_err(|e| ScanError {
            message: format!("{e:#}"),
            line: None,
            column: None,
        })?;
        for (outline, translation) in &entries {
            each(outline, Some(translation));
        }
        return Ok(());
    }
    read_entries(&decode_text(bytes), |key, value| each(key, value)).map_err(|e| ScanError {
        message: format!("not a JSON dictionary: {e}"),
        line: Some(e.line()),
        column: Some(e.column()),
    })
}

struct ScanError {
    message: String,
    line: Option<usize>,
    column: Option<usize>,
}

/// The notation a JSON dictionary's keys are spelled in, when its theory ships one that says so
/// beside it (see [`super::notation::Notation::for_keys_in`]): its keys are read with it, as the
/// daemon reads them.
fn spelled_keys(file: &Path, layout: &Layout) -> Option<super::notation::Notation> {
    if !file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    {
        return None;
    }
    super::notation::Notation::for_keys_in(file.parent()?, layout).map(|(notation, _)| notation)
}

/// Per kind and file, how many findings were held back past [`CAP`].
#[derive(Default)]
struct Capped(HashMap<(Kind, Severity), usize>);

/// Check one file, adding findings to `out`; with `cross`, the checks that compare files too
/// (the `u32` is the file's place in the order). Returns how many entries were valid.
fn check_file_into(
    file: &Path,
    layout: &Layout,
    rules: &Rules,
    mut cross: Option<(&mut Cross, u32)>,
    out: &mut Vec<Finding>,
) -> usize {
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) => {
            out.push(Finding::new(
                file,
                Severity::Error,
                Kind::UnreadableFile,
                format!("cannot be read: {e}"),
            ));
            return 0;
        }
    };
    let first_new = out.len();
    let here = Where {
        rules,
        trusted: rules.trust.is_trusted(file),
    };
    let mut entries = 0;
    let mut kept: HashMap<(Kind, Severity), usize> = HashMap::new();
    let mut capped = Capped::default();
    // The legacy-spelling notes are one finding per kind and file: how many, and the first
    let mut legacy: Vec<(Kind, usize, String, String, Option<String>)> = Vec::new();
    let mut push = |finding: Finding, out: &mut Vec<Finding>| {
        let slot = kept.entry((finding.kind, finding.severity)).or_default();
        if *slot < CAP {
            *slot += 1;
            out.push(finding);
        } else {
            *capped
                .0
                .entry((finding.kind, finding.severity))
                .or_default() += 1;
        }
    };
    let keys = spelled_keys(file, layout);
    let result = scan(file, &bytes, |key, value| {
        let parsed = keys.as_ref().map_or_else(
            || layout.parse_strokes(key),
            |notation| notation.parse_strokes(key),
        );
        let Some(strokes) = parsed else {
            let mut finding = Finding::new(
                file,
                Severity::Error,
                Kind::BadOutline,
                "the outline is not valid for this layout, so the entry is skipped".to_string(),
            );
            finding.outline = Some(key.to_string());
            push(finding, out);
            return;
        };
        let Some(text) = value else {
            let mut finding = Finding::new(
                file,
                Severity::Error,
                Kind::BadValue,
                "the value is not a string, so the entry is skipped".to_string(),
            );
            finding.outline = Some(key.to_string());
            push(finding, out);
            return;
        };
        entries += 1;
        if let Some((cross, index)) = cross.as_mut() {
            let mut hasher = DefaultHasher::new();
            strokes.hash(&mut hasher);
            let hash = hasher.finish();
            match cross.first.get(&hash) {
                None => {
                    cross.first.insert(hash, *index);
                }
                Some(&first) if first == *index => {
                    let mut finding = Finding::new(
                        file,
                        Severity::Warning,
                        Kind::DuplicateOutline,
                        "defined more than once in this file; the last one wins".to_string(),
                    );
                    finding.outline = Some(key.to_string());
                    push(finding, out);
                }
                Some(&first) => {
                    let slot = cross
                        .shadows
                        .entry((*index, first))
                        .or_insert_with(|| (0, key.to_string()));
                    slot.0 += 1;
                }
            }
            if strokes.len() > 1 {
                if let Some(stroke) = strokes.iter().find(|s| cross.transparent.contains(s)) {
                    let mut finding = Finding::new(
                        file,
                        Severity::Warning,
                        Kind::UnreachableOutline,
                        format!(
                            "contains {}, a transparent stroke that never enters the history, so this outline can never match",
                            layout.render_stroke(*stroke)
                        ),
                    );
                    finding.outline = Some(key.to_string());
                    push(finding, out);
                }
            }
        }
        if text.contains('{') || text.starts_with('=') {
            for problem in check_text_in(text, layout, &here) {
                if matches!(problem.kind, Kind::LegacyAlias | Kind::LegacyNoEffect) {
                    match legacy.iter_mut().find(|note| note.0 == problem.kind) {
                        Some(note) => note.1 += 1,
                        None => legacy.push((
                            problem.kind,
                            1,
                            key.to_string(),
                            problem.message,
                            problem.suggestion,
                        )),
                    }
                    continue;
                }
                let mut finding =
                    Finding::new(file, problem.severity, problem.kind, problem.message);
                finding.outline = Some(key.to_string());
                finding.suggestion = problem.suggestion;
                push(finding, out);
            }
        }
    });
    if let Err(e) = result {
        let mut finding = Finding::new(file, Severity::Error, Kind::UnparsableFile, e.message);
        finding.line = e.line;
        finding.column = e.column;
        finding.suggestion = Some("the previous dictionary stays in use until this parses".into());
        out.push(finding);
    }
    for (kind, count, outline, message, suggestion) in legacy {
        let (what, rest) = if kind == Kind::LegacyAlias {
            (
                "legacy spellings of native commands",
                "write the native name",
            )
        } else {
            (
                "legacy commands that have no effect in keymux",
                "they can be dropped",
            )
        };
        let more = if count == 1 {
            String::new()
        } else {
            format!("{count} entries are {what} ({rest}); the first: ")
        };
        let mut finding = Finding::new(file, Severity::Info, kind, format!("{more}{message}"));
        finding.outline = Some(outline);
        finding.suggestion = suggestion;
        finding.count = Some(count);
        out.push(finding);
    }
    if out.len() > first_new && !is_rtf(file) {
        locate(&decode_text(&bytes), &mut out[first_new..]);
    }
    let mut over: Vec<_> = capped.0.into_iter().collect();
    over.sort_by_key(|((kind, severity), _)| (kind.code(), *severity));
    for ((kind, severity), more) in over {
        let mut finding = Finding::new(
            file,
            severity,
            kind,
            format!("and {more} more findings of this kind in this file (only {CAP} are listed)"),
        );
        finding.count = Some(more);
        out.push(finding);
    }
    entries
}

/// A problem in an entry's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub severity: Severity,
    pub kind: Kind,
    pub message: String,
    pub suggestion: Option<String>,
}

const fn problem(
    severity: Severity,
    kind: Kind,
    message: String,
    suggestion: Option<String>,
) -> Problem {
    Problem {
        severity,
        kind,
        message,
        suggestion,
    }
}

/// The `{...}` pieces of an entry's text, without their braces, split the way the formatter
/// splits them (an escaped `\{` or `\}` is a plain character); or why the braces don't pair up.
fn brace_atoms(text: &str) -> Result<Vec<&str>, String> {
    let mut atoms = Vec::new();
    let mut open: Option<usize> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some((_, '{' | '}'))) => {
                chars.next();
            }
            '{' => {
                if open.is_some() {
                    return Err(
                        "a `{` inside another `{...}`; escape a literal brace as \\{".into(),
                    );
                }
                open = Some(at);
            }
            '}' => match open.take() {
                Some(start) => atoms.push(&text[start + 1..at]),
                None => {
                    return Err("a `}` with no `{` before it; escape a literal brace as \\}".into())
                }
            },
            _ => {}
        }
    }
    if open.is_some() {
        return Err("a `{` that is never closed".into());
    }
    Ok(atoms)
}

/// What is wrong with an entry's text: braces that don't pair up, metas the engine doesn't know,
/// and arguments a meta can't use. Nothing for text that is fine.
#[must_use]
pub fn check_text(text: &str, layout: &Layout) -> Vec<Problem> {
    check_text_in(
        text,
        layout,
        &Where {
            rules: &Rules::default(),
            trusted: false,
        },
    )
}

fn check_text_in(text: &str, layout: &Layout, here: &Where) -> Vec<Problem> {
    let mut problems = Vec::new();
    let atoms = match brace_atoms(text) {
        Ok(atoms) => atoms,
        Err(why) => {
            problems.push(problem(
                Severity::Error,
                Kind::UnbalancedBraces,
                format!("unbalanced braces: {why}"),
                None,
            ));
            return problems;
        }
    };
    let whole = mapping_to_macro(Some(text), 0);
    if let Some(found) = &whole {
        // `=name` and `{*}`'s kin: a name no macro has does nothing
        if !macro_name::ALL.contains(&found.name.as_str()) && text.starts_with('=') {
            let suggestion = closest(&found.name, macro_name::ALL.iter().copied())
                .map(|name| text.replacen(found.name.as_str(), name, 1));
            problems.push(problem(
                Severity::Warning,
                Kind::UnknownMeta,
                format!("`{}` is not a macro, so the entry does nothing", found.name),
                suggestion,
            ));
        }
    }
    for atom in atoms {
        check_atom(atom, whole.is_some(), layout, here, &mut problems);
    }
    problems
}

fn check_atom(atom: &str, whole: bool, layout: &Layout, here: &Where, problems: &mut Vec<Problem>) {
    let unescaped = atom.replace("\\{", "{").replace("\\}", "}");
    let shown = format!("{{{atom}}}");
    let Some(found) = meta::parse_meta(&unescaped) else {
        // Nothing to say about whitespace, the empty `{}`, or the legacy `{*...}` macros
        if unescaped.trim().is_empty() || unescaped.starts_with('*') {
            return;
        }
        let suggestion = closest(&unescaped, KNOWN.iter().map(|known| known.name))
            .map(|name| format!("{{:{name}}}"));
        problems.push(problem(
            Severity::Warning,
            Kind::UnknownMeta,
            format!("`{shown}` is not a meta the engine knows, so it types nothing"),
            suggestion,
        ));
        return;
    };
    legacy_notes(&unescaped, &shown, &found, problems);
    if let Some((kind, arg)) = run_control::classify(&found.name, found.param) {
        check_command(kind, &arg, &shown, here, problems);
        return;
    }
    let mut name = found.name.to_lowercase();
    let mut param = found.param;
    // `{:undo_or_ctrl+backspace}`: the combination written into the name of a whole entry
    if whole && name.starts_with("undo_or_") {
        param = &unescaped[":undo_or_".len()..];
        name = "undo_or".to_string();
    }
    let Some(known) = meta::find_meta(&name) else {
        let names = KNOWN.iter().map(|known| known.name);
        let suggestion = closest(&name, names).map(|better| {
            if found.param.is_empty() {
                format!("{{:{better}}}")
            } else {
                format!("{{:{better}:{}}}", found.param)
            }
        });
        problems.push(problem(
            Severity::Warning,
            Kind::UnknownMeta,
            format!("`{shown}` is not a meta the engine knows, so it does nothing"),
            suggestion,
        ));
        return;
    };
    if known.whole_entry && !whole {
        problems.push(problem(
            Severity::Warning,
            Kind::MisplacedMeta,
            format!(
                "`{shown}` only works as an entry's whole text; inside other text it does nothing"
            ),
            None,
        ));
        return;
    }
    let wanted = param.trim().to_lowercase();
    if known.verbs.is_empty() {
        if let Some(why) = (known.check)(param, layout) {
            // The engine commands are the one list of names an argument can be a typo of
            let suggestion = (known.name == "command")
                .then(|| {
                    let command = param.split(':').next().unwrap_or_default().trim();
                    closest(command, meta::ENGINE_COMMANDS).map(|better| {
                        let fixed = param.replacen(command, better, 1);
                        meta::native_spelling(&fixed)
                            .unwrap_or_else(|| format!("{{:command:{fixed}}}"))
                    })
                })
                .flatten();
            problems.push(problem(
                Severity::Warning,
                Kind::BadMetaArgument,
                format!("`{shown}`: {why}"),
                suggestion,
            ));
        }
        return;
    }
    if wanted.is_empty() {
        if !known.bare_ok {
            problems.push(problem(
                Severity::Warning,
                Kind::BadMetaArgument,
                format!(
                    "`{shown}` needs one of: {}; as it is it does nothing",
                    known.verbs.join(", ")
                ),
                None,
            ));
        }
        return;
    }
    if !known.verbs.contains(&wanted.as_str()) {
        let suggestion = closest(&wanted, known.verbs.iter().copied())
            .map(|verb| format!("{{:{}:{verb}}}", known.name));
        problems.push(problem(
            Severity::Warning,
            Kind::BadMetaArgument,
            format!(
                "`{shown}`: `{}` is not one of {}, so it does nothing",
                param.trim(),
                known.verbs.join(", ")
            ),
            suggestion,
        ));
    }
}

/// The notes on a legacy spelling: `{PLOVER:X}` that has a native name says which, and a legacy
/// command keymux has no counterpart for (also as `{:command:X}`) says it does nothing.
fn legacy_notes(text: &str, shown: &str, found: &meta::Meta, problems: &mut Vec<Problem>) {
    if !found.name.eq_ignore_ascii_case("command") {
        return;
    }
    let name = found
        .param
        .split(':')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if meta::NO_EFFECT.contains(&name.as_str()) {
        problems.push(problem(
            Severity::Info,
            Kind::LegacyNoEffect,
            format!("`{shown}` has no effect in keymux (legacy Plover command)"),
            None,
        ));
    } else if meta::strip_prefix_ignore_case(text, "PLOVER:").is_some() {
        if let Some(native) = meta::native_spelling(found.param) {
            problems.push(problem(
                Severity::Info,
                Kind::LegacyAlias,
                format!("`{shown}` is a legacy alias of {native}"),
                Some(native),
            ));
        }
    }
}

/// `{:run:NAME}` and the inline shell forms: an argument is needed, a name must be one the config
/// defines (with a config), and a shell command runs only from a trusted file.
fn check_command(
    kind: run_control::Kind,
    arg: &str,
    shown: &str,
    here: &Where,
    problems: &mut Vec<Problem>,
) {
    if arg.trim().is_empty() {
        let what = if kind == run_control::Kind::Run {
            "a command name"
        } else {
            "a command line"
        };
        problems.push(problem(
            Severity::Warning,
            Kind::BadMetaArgument,
            format!("`{shown}` needs {what}, so it does nothing"),
            None,
        ));
        return;
    }
    match kind {
        run_control::Kind::Run => {
            let Some(commands) = here.rules.commands.as_ref().filter(|commands| !commands.contains_key(arg)) else {
                return;
            };
            let suggestion = closest(arg, commands.keys().map(String::as_str)).map(|name| format!("{{:run:{name}}}"));
            let known = if commands.is_empty() { "none are defined".to_string() } else { format!("defined: {}", sorted_names(commands)) };
            problems.push(problem(
                Severity::Warning,
                Kind::UnknownCommand,
                format!("`{shown}`: no command `{arg}` in the config's commands ({known}), so it does nothing"),
                suggestion,
            ));
        }
        run_control::Kind::Shell if here.trusted => problems.push(problem(
            Severity::Info,
            Kind::ShellCommand,
            format!("`{shown}` runs a shell command as the user (this file is trusted)"),
            None,
        )),
        run_control::Kind::Shell => problems.push(problem(
            Severity::Warning,
            Kind::UntrustedShell,
            format!("`{shown}` is ignored: not a trusted dictionary; trusted: {}", here.rules.trust.describe()),
            Some("name the command in the config's `commands` and use {:run:NAME}, or trust this file with trusted_dictionaries".to_string()),
        )),
    }
}

fn sorted_names(commands: &HashMap<String, String>) -> String {
    let mut names: Vec<&str> = commands.keys().map(String::as_str).collect();
    names.sort_unstable();
    names.join(", ")
}

/// The candidate nearest to `word` by edit distance, if it is near enough to be a typo.
#[must_use]
pub fn closest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let word = word.to_lowercase();
    let limit = (word.chars().count() / 3).clamp(1, 3);
    candidates
        .into_iter()
        .map(|candidate| (distance(&word, candidate), candidate))
        .filter(|(distance, _)| *distance <= limit)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// Edit distance between two words: insertions, deletions, substitutions and swapping two
/// neighbouring letters (the commonest typo) each cost one.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut table = vec![vec![0_usize; b.len() + 1]; a.len() + 1];
    for (i, row) in table.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in table[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let same = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (table[i - 1][j] + 1)
                .min(table[i][j - 1] + 1)
                .min(table[i - 1][j - 1] + same);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(table[i - 2][j - 2] + 1);
            }
            table[i][j] = best;
        }
    }
    table[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;
    use std::fs;

    fn folder(name: &str) -> TestDir {
        TestDir::new(&format!("keymux-validate-{name}"))
    }

    fn put(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    fn plover() -> Layout {
        Layout::plover()
    }

    fn findings_of(name: &str, json: &str) -> Vec<Finding> {
        let dir = folder(name);
        let file = put(&dir, "a.json", json);
        let (findings, _) = validate_files(&[file], &plover());
        findings
    }

    fn one(text: &str) -> Problem {
        let mut found = check_text(text, &plover());
        assert_eq!(found.len(), 1, "{text}: {found:?}");
        found.remove(0)
    }

    /// A dictionary as a theory ships one: words, suffixes, controls, a modifier chord.
    const CLEAN: &str = r#"{
        "KAT": "cat", "TKOG": "dog", "KAT/TKOG": "cat dog", "-G": "{^ing}", "-S": "{^s}",
        "H-L": "{:firstup}{^}", "TKPWAU": "{-|}", "R-R": "{^}\n{-|}", "TP-PL": "{.}", "PW-PB": "{,}",
        "STPH-FRPBLGTS": "{:firstup:except=-FRPBLGTS}{^}{#Super_L(1)}{^}",
        "SKWR*U": "{:outline:last}", "SKWR*E": "{:outline:next}", "SKWRA": "{:lookup}",
        "SKWRO": "{:lookup:open}", "SKWRU": "{:lookup:toggle}", "T*PL": "{:firstup}{:transparent}{:tape:toggle}",
        "TA*PL": "{:tape:clear}", "KWR*PL": "{:overlay:hide}", "KPWR*PL": "{:keyboard:show}",
        "PHRO*RPB": "{:outline:last}", "*": "=undo", "*U": "{:undo_or:ctrl+backspace}",
        "*E": "{:undo_or:Control_L(BackSpace)}", "*EU": "{:undo_or_ctrl+backspace}", "*P": "{:undo_or}",
        "R-RB": "{:redo}", "-RB": "{:undo:2}", "SKP": "{:engine:toggle}", "SKPR": "{:engine:suspend}", "SKPRA": "{:deleted}",
        "TPH": "{:engine:unload}", "TPHO": "{:back:2}", "SH": "{*}", "SHA": "{*?}",
        "TKA": "{#Return}", "TKAO": "{#Control_L(a)}", "TKAOE": "{&a}", "TKAOEU": "{:case:cap_first_word}",
        "P": "{=[aeiou]/an/a}", "PH": "{:stitch:A}", "KW": "\\{literal\\}", "KWR": "", "TK": "{:barrier}"
    }"#;

    #[test]
    fn a_clean_dictionary_has_no_findings() {
        let found = findings_of("clean", CLEAN);
        assert!(found.is_empty(), "clean dictionary was flagged: {found:#?}");
    }

    #[test]
    fn entry_findings_carry_the_line_and_column_of_their_key() {
        let json = "{\n  \"KAT\": \"cat\",\n  \"TKOG\": \"{:tape:togle}\",\n\n    \"A\\\"B\": \"x\",\n  \"v\": \"TKOG\",\n  \"TKOG\": \"{:tape:togle}\"\n}";
        let found = findings_of("lines", json);
        let at = |outline: &str| {
            let hit = found.iter().find(|f| f.outline.as_deref() == Some(outline));
            hit.map(|f| (f.line, f.column))
        };
        // The first key of that name, not a value that spells the same, and 1-based columns
        assert_eq!(at("TKOG"), Some((Some(3), Some(3))), "{found:#?}");
        // A key with an escaped quote is found by its decoded text
        assert_eq!(at("A\"B"), Some((Some(5), Some(5))), "{found:#?}");
        // Whole-file findings have no outline and no invented place
        assert!(
            found
                .iter()
                .all(|f| f.outline.is_none() || f.line.is_some()),
            "{found:#?}"
        );
    }

    #[test]
    fn a_file_that_does_not_parse_is_an_error_with_its_place() {
        let found = findings_of(
            "unparsable",
            "{\n  \"KAT\": \"cat\"\n  \"TKOG\": \"dog\"\n}",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].kind, found[0].severity),
            (Kind::UnparsableFile, Severity::Error)
        );
        assert_eq!(found[0].line, Some(3));
        assert!(found[0].column.is_some());
        // Not an object, truncated, binary and empty files do not panic either
        for text in ["[1, 2]", "{\"KAT\": \"cat\"", "", "\u{0}\u{1}garbage"] {
            let found = findings_of("garbage", text);
            assert!(
                found.iter().any(|f| f.kind == Kind::UnparsableFile),
                "{text:?}: {found:?}"
            );
        }
        let dir = folder("rtf");
        let file = put(&dir, "a.rtf", "this is not rtf");
        let (found, _) = validate_files(&[file], &plover());
        assert_eq!(found[0].kind, Kind::UnparsableFile, "{found:?}");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_error() {
        let dir = folder("missing");
        let (found, _) = validate_files(&[dir.join("nothing.json")], &plover());
        assert_eq!(found[0].kind, Kind::UnreadableFile, "{found:?}");
    }

    #[test]
    fn outlines_and_values_the_loader_skips_are_errors() {
        let found = findings_of(
            "outlines",
            r#"{"KATZ9": "cat", "KAT": 5, "TKOG": "dog", "": "x"}"#,
        );
        let kinds: Vec<_> = found
            .iter()
            .map(|f| (f.kind, f.outline.as_deref()))
            .collect();
        assert_eq!(
            kinds,
            [
                (Kind::BadOutline, Some("KATZ9")),
                (Kind::BadValue, Some("KAT")),
                (Kind::BadOutline, Some("")),
            ]
        );
        assert!(found.iter().all(|f| f.severity == Severity::Error));
    }

    #[test]
    fn braces_that_do_not_pair_up_are_errors() {
        for text in ["{^ing", "ing}", "{a{b}}", "{}{", "x}y{z}"] {
            let found = check_text(text, &plover());
            assert_eq!(found.len(), 1, "{text}");
            assert_eq!(
                (found[0].kind, found[0].severity),
                (Kind::UnbalancedBraces, Severity::Error),
                "{text}"
            );
        }
        for text in [r"\{literal\}", "{^ing}", "{}", "a {^b} c {&d}", r"{^a\}b}"] {
            assert!(check_text(text, &plover()).is_empty(), "{text}");
        }
    }

    #[test]
    fn an_unknown_meta_is_a_warning_with_a_suggestion() {
        let wrong = one("{:tpae:toggle}");
        assert_eq!(
            (wrong.kind, wrong.severity),
            (Kind::UnknownMeta, Severity::Warning)
        );
        assert_eq!(wrong.suggestion.as_deref(), Some("{:tape:toggle}"));
        assert_eq!(one("{:firstupp}").suggestion.as_deref(), Some("{:firstup}"));
        assert_eq!(
            one("{:transparnt}").suggestion.as_deref(),
            Some("{:transparent}")
        );
        // Not close to anything: still reported, nothing suggested
        let far = one("{:zzzzzzzz}");
        assert_eq!((far.kind, far.suggestion), (Kind::UnknownMeta, None));
        // Not a meta at all, so it types nothing
        let bare = one("{firstup}");
        assert_eq!(
            (bare.kind, bare.suggestion.as_deref()),
            (Kind::UnknownMeta, Some("{:firstup}"))
        );
        assert_eq!(one("{!'}").kind, Kind::UnknownMeta);
        // A macro name that does not exist
        let macro_typo = one("=undoo");
        assert_eq!(
            (macro_typo.kind, macro_typo.suggestion.as_deref()),
            (Kind::UnknownMeta, Some("=undo"))
        );
    }

    #[test]
    fn a_known_meta_with_a_bad_verb_is_a_warning_with_a_suggestion() {
        let togle = one("{:tape:togle}");
        assert_eq!(
            (togle.kind, togle.severity),
            (Kind::BadMetaArgument, Severity::Warning)
        );
        assert_eq!(togle.suggestion.as_deref(), Some("{:tape:toggle}"));
        assert_eq!(
            one("{:outline:lst}").suggestion.as_deref(),
            Some("{:outline:last}")
        );
        assert_eq!(
            one("{:lookup:opn}").suggestion.as_deref(),
            Some("{:lookup:open}")
        );
        assert_eq!(
            one("{:engine:unlaod}").suggestion.as_deref(),
            Some("{:engine:unload}")
        );
        for text in [
            "{:outline:foo}",
            "{:lookup:bar}",
            "{:tape}",
            "{:overlay:clear}",
            "{:keyboard:}",
            "{:engine:}",
        ] {
            assert_eq!(one(text).kind, Kind::BadMetaArgument, "{text}");
        }
        assert_eq!(
            one("{PLOVER:SUSPND}").suggestion.as_deref(),
            Some("{:engine:suspend}")
        );
        assert_eq!(
            one("{PLOVER:FOKUS}").suggestion.as_deref(),
            Some("{:command:focus}")
        );
        assert_eq!(
            one("{:engine:togle}").suggestion.as_deref(),
            Some("{:engine:toggle}")
        );
    }

    #[test]
    fn legacy_spellings_name_their_native_command() {
        for (text, native) in [
            ("{PLOVER:TOGGLE}", "{:engine:toggle}"),
            ("{plover:suspend}", "{:engine:suspend}"),
            ("{PLOVER:LOOKUP}", "{:lookup}"),
            ("{PLOVER:RUN:pw}", "{:run:pw}"),
            ("{plover:deleted}", "{:deleted}"),
        ] {
            let problem = one(text);
            assert_eq!(
                (problem.kind, problem.severity),
                (Kind::LegacyAlias, Severity::Info),
                "{text}"
            );
            assert_eq!(problem.suggestion.as_deref(), Some(native), "{text}");
            assert!(
                problem
                    .message
                    .contains(&format!("legacy alias of {native}")),
                "{}",
                problem.message
            );
        }
        for native in [
            "{:engine:toggle}",
            "{:engine:suspend}",
            "{:lookup}",
            "{:run:pw}",
            "{:deleted}",
            "{:command:toggle}",
        ] {
            assert!(check_text(native, &plover()).is_empty(), "{native}");
        }
    }

    #[test]
    fn legacy_commands_with_no_counterpart_have_no_effect() {
        for text in [
            "{PLOVER:FOCUS}",
            "{plover:quit}",
            "{PLOVER:ADD_TRANSLATION}",
            "{:command:resume}",
        ] {
            let problem = one(text);
            assert_eq!(
                (problem.kind, problem.severity),
                (Kind::LegacyNoEffect, Severity::Info),
                "{text}"
            );
            assert!(
                problem
                    .message
                    .contains("has no effect in keymux (legacy Plover command)"),
                "{}",
                problem.message
            );
        }
        // set_config sets session options, so it is no no-op; a command nobody has is an unknown one
        assert!(
            check_text("{PLOVER:SET_CONFIG:'start_attached':True}", &plover()).is_empty(),
            "set_config is a real command"
        );
        assert_eq!(one("{PLOVER:BOGUS}").kind, Kind::BadMetaArgument);
    }

    #[test]
    fn legacy_notes_are_one_finding_per_kind_and_file_with_a_count() {
        let json = r#"{"A": "{PLOVER:TOGGLE}", "E": "{plover:suspend}", "O": "{PLOVER:FOCUS}", "U": "{PLOVER:QUIT}", "AO": "{:engine:toggle}"}"#;
        let found = findings_of("legacy", json);
        assert_eq!(found.len(), 2, "{found:#?}");
        let alias = found.iter().find(|f| f.kind == Kind::LegacyAlias).unwrap();
        assert_eq!(
            (alias.count, alias.outline.as_deref(), alias.severity),
            (Some(2), Some("A"), Severity::Info)
        );
        assert!(
            alias.message.contains("2 entries")
                && alias.message.contains("legacy alias of {:engine:toggle}"),
            "{}",
            alias.message
        );
        let none = found
            .iter()
            .find(|f| f.kind == Kind::LegacyNoEffect)
            .unwrap();
        assert_eq!((none.count, none.line), (Some(2), Some(1)));
        assert_eq!(
            found.iter().filter(|f| f.severity > Severity::Info).count(),
            0
        );
    }

    #[test]
    fn firstup_arguments_are_checked_against_the_layout() {
        for text in [
            "{:firstup:except=}",
            "{:firstup:except=ZZZ}",
            "{:firstup:except=T/K}",
            "{:firstup:foo}",
            "{:firstup:except}",
        ] {
            assert_eq!(one(text).kind, Kind::BadMetaArgument, "{text}");
        }
        for text in [
            "{:firstup}",
            "{:firstup:except=-FRPBLGTS}",
            "{:firstup:except=STPH*}",
            "{:FirstUp}",
        ] {
            assert!(check_text(text, &plover()).is_empty(), "{text}");
        }
        // An extra key the layout has is a key; one it lacks is not
        let with_plus = Layout::plover_with_joins(&['+'], &['+']).unwrap();
        let found = check_text("{:firstup:except=+T}", &with_plus);
        assert!(
            found.is_empty(),
            "an extra key of the layout is a key: {found:?}"
        );
        assert_eq!(check_text("{:firstup:except=+T}", &plover()).len(), 1);
    }

    #[test]
    fn key_combinations_and_undo_arguments_are_checked() {
        assert_eq!(one("{#Bogus_Key}").kind, Kind::BadMetaArgument);
        assert_eq!(one("{#Control_L(a}").kind, Kind::BadMetaArgument);
        assert_eq!(one("{:undo_or:Contrl_L(a)}").kind, Kind::BadMetaArgument);
        assert_eq!(one("{:undo_or_ctrl+bogus}").kind, Kind::BadMetaArgument);
        assert_eq!(one("{:undo:many}").kind, Kind::BadMetaArgument);
        assert_eq!(one("{:back:x}").kind, Kind::BadMetaArgument);
        assert_eq!(one("{:transparent:yes}").kind, Kind::BadMetaArgument);
        assert_eq!(one("{:case:shouting}").kind, Kind::BadMetaArgument);
        for text in [
            "{#Super_L(1)}",
            "{#control(z shift(z))}",
            "{#AudioPlay}",
            "{#control(AudioNext)}",
            "{#XF86AudioStop}",
            "{:undo_or:ctrl+backspace}",
            "{:undo_or_ctrl+backspace}",
        ] {
            assert!(check_text(text, &plover()).is_empty(), "{text}");
        }
    }

    #[test]
    fn a_macro_inside_other_text_does_nothing() {
        let inside = one("word{:undo}");
        assert_eq!(
            (inside.kind, inside.severity),
            (Kind::MisplacedMeta, Severity::Warning)
        );
        let whole = check_text("{:undo}", &plover());
        assert!(
            whole.is_empty(),
            "a macro as the whole entry is fine: {whole:?}"
        );
    }

    #[test]
    fn the_same_outline_twice_in_one_file_is_a_warning() {
        let found = findings_of("dupes", r#"{"KAT": "cat", "TKOG": "dog", "KAT": "kat"}"#);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (
                found[0].kind,
                found[0].severity,
                found[0].outline.as_deref()
            ),
            (Kind::DuplicateOutline, Severity::Warning, Some("KAT"))
        );
    }

    #[test]
    fn an_outline_in_two_files_is_a_note_not_a_warning() {
        let dir = folder("shadow");
        let first = put(&dir, "01.json", r#"{"KAT": "cat", "TKOG": "dog"}"#);
        let second = put(
            &dir,
            "02.json",
            r#"{"KAT": "kitty", "TKOG": "hound", "PWAO": "boo"}"#,
        );
        let (found, entries) = validate_files(&[first.clone(), second.clone()], &plover());
        assert_eq!(entries, 5);
        assert_eq!(found.len(), 1, "{found:?}");
        let note = &found[0];
        assert_eq!(
            (note.kind, note.severity, note.count),
            (Kind::ShadowedOutline, Severity::Info, Some(2))
        );
        assert_eq!(note.file, second.display().to_string());
        assert!(
            note.message.contains(&first.display().to_string()),
            "{}",
            note.message
        );
        assert_eq!(note.outline.as_deref(), Some("KAT"));
    }

    #[test]
    fn a_multi_stroke_outline_through_a_transparent_stroke_can_never_match() {
        let dir = folder("transparent");
        let first = put(
            &dir,
            "01.json",
            r#"{"PWR": "{:transparent}", "SKWRA": "{:lookup}", "STPH": "{:tape:toggle}"}"#,
        );
        let second = put(
            &dir,
            "02.json",
            r#"{"PWR/KAT": "bad", "KAT/PWR": "bad", "KAT/SKWRA": "bad", "STPH/KAT": "bad", "KAT/TKOG": "fine", "PWR": "word"}"#,
        );
        let (found, _) = validate_files(&[first, second], &plover());
        let unreachable: Vec<_> = found
            .iter()
            .filter(|f| f.kind == Kind::UnreachableOutline)
            .map(|f| f.outline.clone().unwrap())
            .collect();
        assert_eq!(unreachable, ["PWR/KAT", "KAT/PWR", "KAT/SKWRA", "STPH/KAT"]);
        assert!(found
            .iter()
            .filter(|f| f.kind == Kind::UnreachableOutline)
            .all(|f| f.severity == Severity::Warning));
        // A higher-priority file that makes the stroke a word leaves its outlines reachable
        let dir = folder("transparent-won");
        let first = put(&dir, "01.json", r#"{"PWR": "word"}"#);
        let second = put(
            &dir,
            "02.json",
            r#"{"PWR": "{:transparent}", "PWR/KAT": "fine"}"#,
        );
        let (found, _) = validate_files(&[first, second], &plover());
        assert!(
            found.iter().all(|f| f.kind != Kind::UnreachableOutline),
            "{found:?}"
        );
    }

    #[test]
    fn findings_of_one_kind_in_one_file_are_capped_with_a_count() {
        let entries: Vec<String> = (0..CAP + 25)
            .map(|n| format!("\"BAD{n}9\": \"x\""))
            .collect();
        let found = findings_of("cap", &format!("{{{}}}", entries.join(",")));
        assert_eq!(found.len(), CAP + 1);
        let last = found.last().unwrap();
        assert_eq!(last.count, Some(25));
        assert!(last.message.contains("25 more"), "{}", last.message);
    }

    #[test]
    fn disabled_files_plugins_and_the_word_list_are_not_dictionaries() {
        let dir = folder("walk");
        put(&dir, "01.json", r#"{"KAT": "cat"}"#);
        put(&dir, "disabled_old.json", "not json");
        put(&dir, "plugin.rs", "fn not_json() {}");
        put(&dir, WORDLIST_FILE, "word\nlist");
        let target = Target {
            layers: vec!["steno".into()],
            roots: vec![dir.to_path_buf()],
            layout: plover(),
            rules: Rules::default(),
        };
        let report = validate(&[target], &dir.join("cache"));
        assert!(report.findings.is_empty(), "{:#?}", report.findings);
        assert_eq!((report.files, report.entries), (1, 1));
    }

    #[test]
    fn the_same_file_under_two_layers_is_reported_once_with_both() {
        let dir = folder("layers");
        put(&dir, "01.json", r#"{"KAT": "{:tape:togle}"}"#);
        let target = |name: &str| Target {
            layers: vec![name.to_string()],
            roots: vec![dir.to_path_buf()],
            layout: plover(),
            rules: Rules::default(),
        };
        let with_plus = Target {
            layout: Layout::plover_with_joins(&['+'], &['+']).unwrap(),
            ..target("second")
        };
        let report = validate(&[target("first"), with_plus], &dir.join("cache"));
        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
        assert_eq!(report.findings[0].layers, ["first", "second"]);
        assert!(!report.passes(true) && report.passes(false));
    }

    #[test]
    fn closest_finds_typos_and_nothing_else() {
        assert_eq!(distance("togle", "toggle"), 1);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("tpae", "tape"), 1);
        assert_eq!(closest("togle", ["show", "toggle"]), Some("toggle"));
        assert_eq!(closest("TOGGLE", ["toggle"]), Some("toggle"));
        assert_eq!(closest("xyz", ["toggle", "show"]), None);
        assert_eq!(
            closest("ab", ["xy"]),
            None,
            "short words allow one edit only"
        );
    }

    fn rules(trusted: &[&str], commands: Option<&[&str]>) -> Rules {
        let names: Vec<String> = trusted.iter().map(|name| (*name).to_string()).collect();
        Rules {
            trust: Trust::new(Path::new("/s"), &names),
            commands: commands.map(|names| {
                names
                    .iter()
                    .map(|name| ((*name).to_string(), "x".to_string()))
                    .collect()
            }),
        }
    }

    fn found(text: &str, file: &str, rules: &Rules) -> Vec<Problem> {
        check_text_in(
            text,
            &plover(),
            &Where {
                rules,
                trusted: rules.trust.is_trusted(Path::new(file)),
            },
        )
    }

    #[test]
    fn run_names_are_checked_against_the_config_when_there_is_one() {
        let with = rules(&[], Some(&["password", "lock"]));
        assert_eq!(found("{:run:password}", "/s/a.json", &with), Vec::new());
        let wrong = found("{:run:pasword}", "/s/a.json", &with);
        assert_eq!(wrong.len(), 1, "{wrong:?}");
        assert_eq!(wrong[0].kind, Kind::UnknownCommand);
        assert_eq!(wrong[0].suggestion.as_deref(), Some("{:run:password}"));
        let none = found("{:run:x}", "/s/a.json", &rules(&[], Some(&[])));
        assert!(none[0].message.contains("none are defined"), "{none:?}");
        assert!(
            found("{:run:pasword}", "/s/a.json", &rules(&[], None)).is_empty(),
            "no config: nothing to check against"
        );
        let empty = found("{:run:}", "/s/a.json", &with);
        assert_eq!((empty.len(), empty[0].kind), (1, Kind::BadMetaArgument));
    }

    #[test]
    fn inline_shell_is_a_warning_in_an_untrusted_file_and_info_in_a_trusted_one() {
        let rules = rules(&["00-user.json"], None);
        for text in [
            "{:shell:ls}",
            "{PLOVER:SHELL:ls}",
            "{:command:shell:ls}",
            "{plover:shell:ls}",
        ] {
            let not_legacy = |all: Vec<Problem>| {
                all.into_iter()
                    .filter(|p| p.kind != Kind::LegacyAlias)
                    .collect::<Vec<_>>()
            };
            let bad = not_legacy(found(text, "/s/lyk/a.json", &rules));
            assert_eq!(bad.len(), 1, "{text}: {bad:?}");
            assert_eq!(
                (bad[0].kind, bad[0].severity),
                (Kind::UntrustedShell, Severity::Warning),
                "{text}"
            );
            assert!(
                bad[0]
                    .message
                    .contains("ignored: not a trusted dictionary; trusted: 00-user.json"),
                "{}",
                bad[0].message
            );
            let good = not_legacy(found(text, "/s/00-user.json", &rules));
            assert_eq!(
                (good[0].kind, good[0].severity),
                (Kind::ShellCommand, Severity::Info),
                "{text}"
            );
        }
        let empty = found("{:shell:}", "/s/00-user.json", &rules);
        assert_eq!(empty[0].kind, Kind::BadMetaArgument);
    }

    #[test]
    fn brace_and_escape_mistakes_in_an_inline_command_are_findings() {
        let rules = rules(&["a.json"], None);
        let nested = found("{:shell:echo ${HOME}}", "/s/a.json", &rules);
        assert_eq!(
            (nested.len(), nested[0].kind),
            (1, Kind::UnbalancedBraces),
            "{nested:?}"
        );
        assert!(found(r"{:shell:echo $\{HOME\}}", "/s/a.json", &rules)
            .iter()
            .all(|p| p.kind == Kind::ShellCommand));
        let open = found("{:shell:echo", "/s/a.json", &rules);
        assert_eq!(open[0].kind, Kind::UnbalancedBraces);
    }

    #[test]
    fn a_file_is_judged_by_where_it_is_in_a_whole_validation() {
        let dir = folder("trust-files");
        let steno = dir.join("steno");
        fs::create_dir_all(steno.join("lyk")).unwrap();
        let mine = put(
            &steno,
            "00-user.json",
            r#"{"PWR": "{:run:password}", "SKWR": "{:shell:notify-send hi}"}"#,
        );
        let theirs = put(
            &steno.join("lyk"),
            "01.json",
            r#"{"TKOG": "{:shell:rm x}", "KAT": "{:run:missing}"}"#,
        );
        let rules = Rules {
            trust: Trust::standard(&steno),
            commands: Some(HashMap::from([("password".to_string(), "x".to_string())])),
        };
        let (findings, _) = validate_files_with(&[mine, theirs], &plover(), &rules);
        let kinds: Vec<(String, Kind)> = findings
            .iter()
            .map(|f| (f.file.rsplit('/').next().unwrap().to_string(), f.kind))
            .collect();
        assert!(
            kinds.contains(&("00-user.json".to_string(), Kind::ShellCommand)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("01.json".to_string(), Kind::UntrustedShell)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("01.json".to_string(), Kind::UnknownCommand)),
            "{kinds:?}"
        );
        assert_eq!(kinds.len(), 3, "{kinds:?}");
    }

    #[test]
    fn steno_validate_works_with_no_config_and_trusts_the_default_files() {
        let dir = folder("no-config");
        let targets =
            super::super::validate_cli::targets(&dir.join("absent.ron"), &dir, None, &[]).unwrap();
        assert_eq!(targets.len(), 1);
        assert!(targets[0].rules.commands.is_none());
        assert!(targets[0]
            .rules
            .trust
            .is_trusted(&dir.join("steno/00-user.json")));
        assert!(!targets[0]
            .rules
            .trust
            .is_trusted(&dir.join("steno/lyk/x.json")));
    }

    #[test]
    fn the_known_metas_and_their_verbs_are_what_the_formatter_runs() {
        use super::super::action::Action;
        use super::super::formatter::Context;
        use super::super::orthography::Orthography;
        let orthography = Orthography::default();
        let ctx = Context::new(&[], Action::default(), &orthography);
        for known in KNOWN {
            for verb in known.verbs {
                let action = (known.run)(&ctx, verb);
                assert!(
                    action.command.is_some(),
                    "{{:{}:{verb}}} makes no command",
                    known.name
                );
            }
            if !known.verbs.is_empty() && !known.bare_ok {
                assert!(
                    (known.run)(&ctx, "").command.is_none()
                        && (known.run)(&ctx, "zzz").command.is_none(),
                    "{} accepts a verb it does not list",
                    known.name
                );
            }
        }
        // The names the stroke classes look for are all known metas
        for name in super::super::overlay_control::NAMES {
            assert!(meta::find_meta(name).is_some(), "{name}");
        }
        let engine = include_str!("engine.rs");
        for name in meta::ENGINE_COMMANDS {
            assert!(
                name.starts_with(char::is_alphabetic)
                    && (engine.contains(&format!("\"{name}\"")) || name.ends_with("_control")),
                "engine command {name} is not run by the engine"
            );
        }
    }
}
