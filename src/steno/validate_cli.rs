//! The command-line side of dictionary validation: which files `keymux steno validate` reads,
//! how the findings are shown, and the exit code. The checks themselves are in
//! [`super::validate`].
//!
//! Exit codes of `keymux steno validate`: `0` nothing wrong (warnings and info do not count,
//! unless `--strict` makes warnings count), `1` errors found (or warnings with `--strict`), `2` it
//! could not run (a path given does not exist).

use super::answer::EngineSetup;
use super::layout::Layout;
use super::setup::{cache_dir, default_dictionaries};
use super::validate::{targets_from_config, validate, Finding, Report, Rules, Severity, Target};
use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// What `--json` prints.
#[derive(Serialize)]
struct Output<'a> {
    /// Whether the exit code is 0
    ok: bool,
    /// Whether warnings count as failures
    strict: bool,
    errors: usize,
    warnings: usize,
    info: usize,
    #[serde(flatten)]
    report: &'a Report,
}

/// The dictionaries to check: the files and folders given, or those of the config's steno layers
/// (the whole steno folder when it has none).
///
/// # Errors
/// When a path given does not exist.
pub fn targets(
    config_path: &Path,
    config_dir: &Path,
    home: Option<&Path>,
    given: &[PathBuf],
) -> Result<Vec<Target>> {
    if let Some(missing) = given.iter().find(|path| !path.exists()) {
        anyhow::bail!("{} does not exist", missing.display());
    }
    // Without a config (or one that does not load), the standard trust and no names to check
    let config = crate::config::Config::load_or_hint(config_path).ok();
    let rules = config.as_ref().map_or_else(|| Rules::standard(config_dir), |config| Rules::of_config(config, config_dir));
    if !given.is_empty() {
        let setup = EngineSetup::load(config_path, config_dir, home);
        return Ok(vec![Target {
            layers: Vec::new(),
            roots: given.to_vec(),
            layout: setup.layout,
            rules,
        }]);
    }
    let from_config = config
        .as_ref()
        .map(|config| targets_from_config(config, config_dir, home))
        .unwrap_or_default();
    if !from_config.is_empty() {
        return Ok(from_config);
    }
    Ok(vec![Target {
        layers: vec!["(default)".to_string()],
        roots: default_dictionaries(config_dir),
        layout: Layout::plover(),
        rules,
    }])
}

/// Validate `targets` for the user whose config is in `config_dir`.
#[must_use]
pub fn run(targets: &[Target], config_dir: &Path) -> Report {
    validate(targets, &cache_dir(config_dir))
}

/// The report as the JSON `--json` prints.
///
/// # Errors
/// Never in practice: the report is plain data.
pub fn to_json(report: &Report, strict: bool) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&Output {
        ok: report.passes(strict),
        strict,
        errors: report.count(Severity::Error),
        warnings: report.count(Severity::Warning),
        info: report.count(Severity::Info),
        report,
    })
}

fn paint(severity: Severity, text: &str) -> colored::ColoredString {
    match severity {
        Severity::Error => text.bright_red().bold(),
        Severity::Warning => text.bright_yellow(),
        Severity::Info => text.dimmed(),
    }
}

/// The report for a person: findings grouped by file, each with its severity, outline, message
/// and suggestion, then a count. `indent` is put before every line.
#[must_use]
pub fn to_text(report: &Report, indent: &str) -> String {
    let mut out = String::new();
    let mut current: Option<&str> = None;
    for finding in &report.findings {
        if current != Some(finding.file.as_str()) {
            current = Some(finding.file.as_str());
            out.push_str(&format!("{indent}{}\n", finding.file.bold()));
        }
        out.push_str(&finding_text(finding, indent));
    }
    let (errors, warnings, info) = (
        report.count(Severity::Error),
        report.count(Severity::Warning),
        report.count(Severity::Info),
    );
    out.push_str(&format!(
        "{indent}{} files, {} entries: {} error(s), {} warning(s), {} note(s)\n",
        report.files, report.entries, errors, warnings, info
    ));
    out
}

fn finding_text(finding: &Finding, indent: &str) -> String {
    let mut line = format!(
        "{indent}  {} {}",
        paint(finding.severity, &format!("{:<7}", finding.severity.word())),
        finding.kind.code().dimmed()
    );
    if let Some(outline) = &finding.outline {
        line.push_str(&format!(" {}", format!("\"{outline}\"").cyan()));
    }
    // `file:line:column`, which editors and terminals can jump to
    if let Some(at) = finding.line {
        let column = finding.column.map_or_else(String::new, |column| format!(":{column}"));
        line.push_str(&format!(" {}", format!("({}:{at}{column})", finding.file).dimmed()));
    }
    line.push_str(&format!(": {}\n", finding.message));
    if let Some(suggestion) = &finding.suggestion {
        line.push_str(&format!("{indent}          {} {suggestion}\n", "suggestion:".green()));
    }
    if !finding.layers.is_empty() {
        line.push_str(&format!(
            "{indent}          {}\n",
            format!("layers: {}", finding.layers.join(", ")).dimmed()
        ));
    }
    line
}

/// `keymux steno validate`: print the report and return the exit code.
///
/// # Errors
/// When a path given does not exist (exit code 2 for the caller).
pub fn command(
    config_path: &Path,
    config_dir: &Path,
    home: Option<&Path>,
    given: &[PathBuf],
    strict: bool,
    json: bool,
) -> Result<i32> {
    let targets = targets(config_path, config_dir, home, given)?;
    let report = run(&targets, config_dir);
    if json {
        println!("{}", to_json(&report, strict)?);
    } else {
        for set in &report.sets {
            let layers = if set.layers.is_empty() {
                "given files".to_string()
            } else {
                format!("steno layer {}", set.layers.join(", "))
            };
            println!("{} {} file(s)", format!("{layers}:").bold(), set.files.len());
        }
        print!("{}", to_text(&report, ""));
    }
    Ok(i32::from(!report.passes(strict)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keymux-validate-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn exit_code(files: &[PathBuf], strict: bool) -> Result<i32> {
        let nowhere = Path::new("/nonexistent/keymux/config.ron");
        command(nowhere, Path::new("/nonexistent/keymux"), None, files, strict, true)
    }

    #[test]
    fn the_exit_code_is_zero_for_clean_one_for_errors_and_strict_warnings_and_an_error_for_no_such_path() {
        let dir = folder("exit");
        let clean = dir.join("clean.json");
        fs::write(&clean, r#"{"KAT": "cat", "-G": "{^ing}"}"#).unwrap();
        let warned = dir.join("warned.json");
        fs::write(&warned, r#"{"KAT": "{:tape:togle}"}"#).unwrap();
        let broken = dir.join("broken.json");
        fs::write(&broken, r#"{"KAT": "#).unwrap();
        assert_eq!(exit_code(&[clean], true).unwrap(), 0);
        assert_eq!(exit_code(std::slice::from_ref(&warned), false).unwrap(), 0, "warnings alone pass");
        assert_eq!(exit_code(&[warned], true).unwrap(), 1, "--strict fails on warnings");
        assert_eq!(exit_code(&[broken], false).unwrap(), 1);
        assert!(exit_code(&[dir.join("nothing.json")], false).is_err());
    }

    #[test]
    fn the_json_has_a_fixed_shape() {
        let dir = folder("json");
        let file = dir.join("a.json");
        fs::write(&file, r#"{"KAT": "{:tape:togle}", "KATZ9": "x"}"#).unwrap();
        let nowhere = Path::new("/nonexistent/keymux/config.ron");
        let targets = targets(nowhere, nowhere, None, std::slice::from_ref(&file)).unwrap();
        let report = run(&targets, &dir.join("cache"));
        let value: serde_json::Value = serde_json::from_str(&to_json(&report, false).unwrap()).unwrap();
        let object = value.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["entries", "errors", "files", "findings", "info", "ok", "sets", "strict", "warnings"]);
        assert_eq!(object["ok"], false);
        assert_eq!((&object["errors"], &object["warnings"], &object["info"]), (&1.into(), &1.into(), &0.into()));
        assert_eq!(object["sets"][0]["files"][0], file.display().to_string());
        let findings = object["findings"].as_array().unwrap();
        let mut finding_keys: Vec<&str> = findings[0].as_object().unwrap().keys().map(String::as_str).collect();
        finding_keys.sort_unstable();
        assert_eq!(
            finding_keys,
            ["column", "count", "file", "kind", "layers", "line", "message", "outline", "severity", "suggestion"]
        );
        let kinds: Vec<_> = findings.iter().map(|f| (f["severity"].as_str().unwrap(), f["kind"].as_str().unwrap())).collect();
        assert_eq!(kinds, [("error", "bad_outline"), ("warning", "bad_meta_argument")]);
        assert_eq!(findings[1]["suggestion"], "{:tape:toggle}");
    }

    #[test]
    fn the_text_groups_findings_by_file() {
        let dir = folder("text");
        let file = dir.join("a.json");
        fs::write(&file, r#"{"KAT": "{:tape:togle}"}"#).unwrap();
        let nowhere = Path::new("/nonexistent/keymux/config.ron");
        let targets = targets(nowhere, nowhere, None, std::slice::from_ref(&file)).unwrap();
        let text = to_text(&run(&targets, &dir.join("cache")), "");
        // Once as the heading, and once in the `file:line:column` an editor can jump to
        let at = format!("{}:1:2", file.display());
        assert_eq!(text.matches(&file.display().to_string()).count(), 2, "{text}");
        assert!(text.contains(&at), "{text}");
        assert!(text.contains("bad_meta_argument") && text.contains("{:tape:toggle}"), "{text}");
        assert!(text.contains("1 files, 1 entries: 0 error(s), 1 warning(s), 0 note(s)"), "{text}");
    }
}
