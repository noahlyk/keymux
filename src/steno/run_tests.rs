//! `{:run:NAME}`, `{:shell:CMD}` and `{PLOVER:SHELL:CMD}` through a real engine: what they run, what
//! they type and leave in the history, and which dictionary files may run inline commands. A
//! recording runner stands in for the user's processes, so no test starts one.
//!
//! The entries are written `{:run:...}`, markers and not format strings.
#![allow(clippy::literal_string_with_formatting_args)]

use super::unload_tests::Rig;
use super::*;
use crate::steno::journal;
use crate::steno::run_control::tests::Recorder;
use std::fs;

fn stroke_of(text: &str) -> u32 {
    Layout::plover().parse_stroke(text).unwrap()
}

fn commands() -> HashMap<String, String> {
    HashMap::from([
        ("password".to_string(), "/home/u/password.sh".to_string()),
        ("lock".to_string(), "loginctl lock-session".to_string()),
    ])
}

/// An engine over `json`, its dictionary trusted or not, running commands into the recorder.
fn engine(json: &str, trusted: bool) -> (StenoEngine, Arc<Recorder>) {
    let mut dict = Dictionary::default();
    dict.merge_json(json, &Layout::plover()).unwrap();
    let mut engine =
        StenoEngine::for_tests(if trusted { dict.trusted() } else { dict }, &HashMap::new());
    let recorder = Arc::new(Recorder::default());
    engine.launcher = Launcher::new(commands(), recorder.clone());
    engine.hub = Box::leak(Box::default());
    (engine, recorder)
}

fn hit(engine: &mut StenoEngine, outline: &str) -> Option<StenoOutput> {
    engine.translate(stroke_of(outline), false)
}

fn typed(output: Option<StenoOutput>) -> String {
    output
        .map(StenoOutput::into_ops)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|op| match op {
            OutputOp::Text(text) => Some(text),
            _ => None,
        })
        .collect()
}

#[test]
fn run_runs_the_named_command_and_types_nothing() {
    let (mut engine, ran) = engine(r#"{"PWR": "{:run:password}", "KAT": "cat"}"#, false);
    assert_eq!(hit(&mut engine, "PWR"), None);
    assert_eq!(ran.ran(), ["/home/u/password.sh"]);
    assert_eq!(typed(hit(&mut engine, "KAT")).trim(), "cat");
    assert_eq!(ran.ran().len(), 1, "a word runs nothing");
}

#[test]
fn run_works_from_any_dictionary_because_the_config_owns_what_the_name_runs() {
    let (mut engine, ran) = engine(r#"{"PWR": "{:run:lock}"}"#, false);
    hit(&mut engine, "PWR");
    assert_eq!(ran.ran(), ["loginctl lock-session"]);
}

#[test]
fn an_unknown_name_runs_nothing_and_the_stroke_still_does_nothing_else() {
    let (mut engine, ran) = engine(r#"{"PWR": "{:run:nope}", "KAT": "cat"}"#, false);
    assert_eq!(hit(&mut engine, "PWR"), None);
    hit(&mut engine, "PWR");
    assert!(ran.ran().is_empty(), "{:?}", ran.ran());
    assert_eq!(engine.launcher.unknown_count(), 1, "logged once");
}

#[test]
fn a_stroke_of_only_run_commands_is_transparent_on_the_tape_and_not_in_the_history() {
    let (mut engine, _) = engine(
        r#"{"PWR": "{:run:password}", "KAT": "cat", "-S": "{^s}"}"#,
        false,
    );
    let hub = engine.hub;
    hit(&mut engine, "KAT");
    let before = format!("{:?}", journal::lock(&engine.translator.history()).clone());
    assert!(
        engine.translator.is_command(stroke_of("PWR")),
        "never repeats when held"
    );
    hit(&mut engine, "PWR");
    assert!(engine.translator.was_transparent());
    assert_eq!(
        format!("{:?}", journal::lock(&engine.translator.history()).clone()),
        before
    );
    let tape = hub.snapshot().tape;
    assert!(
        tape[1].transparent && tape[1].text.contains("run:password"),
        "{:?}",
        tape[1]
    );
    // The join carries on across it: the suffix still attaches to cat
    hit(&mut engine, "-S");
    assert_eq!(engine.translator.text().trim(), "cats");
}

#[test]
fn firstup_composes_with_run() {
    let (mut engine, ran) = engine(r#"{"PWR": "{:firstup}{:run:password}"}"#, false);
    assert_eq!(engine.translator.first_up_exempt(stroke_of("PWR")), Some(0));
    assert!(engine.translator.is_command(stroke_of("PWR")));
    hit(&mut engine, "PWR");
    assert!(engine.translator.was_transparent());
    assert_eq!(ran.ran(), ["/home/u/password.sh"]);
}

#[test]
fn run_beside_text_types_the_text_and_is_an_ordinary_stroke() {
    let (mut engine, ran) = engine(r#"{"PWR": "{:run:lock}hello"}"#, false);
    assert_eq!(typed(hit(&mut engine, "PWR")).trim(), "hello");
    assert!(!engine.translator.was_transparent());
    assert_eq!(ran.ran(), ["loginctl lock-session"]);
    assert!(
        engine.translator.is_command(stroke_of("PWR")),
        "still never repeats"
    );
}

#[test]
fn a_trusted_dictionary_runs_inline_shell_in_either_spelling_with_braces_unescaped() {
    let (mut engine, ran) = engine(
        r#"{"A": "{:shell:echo \\{hi\\}}", "E": "{PLOVER:SHELL:ls -l}", "O": "{:command:shell:id}", "U": "{PLOVER:SHELL:id}{PLOVER:QUIT}"}"#,
        true,
    );
    for outline in ["A", "E", "O", "U"] {
        assert_eq!(hit(&mut engine, outline), None, "{outline}");
    }
    assert_eq!(ran.ran(), ["echo {hi}", "ls -l", "id", "id"]);
}

#[test]
fn an_untrusted_dictionary_types_nothing_and_runs_nothing_for_inline_shell() {
    let (mut engine, ran) = engine(
        r#"{"A": "{:shell:touch /tmp/x}", "E": "{PLOVER:SHELL:touch /tmp/x}", "O": "{plover:shell:x}", "U": "{:command:SHELL:x}", "KAT": "cat{:shell:x}s"}"#,
        false,
    );
    for outline in ["A", "E", "O", "U"] {
        assert_eq!(typed(hit(&mut engine, outline)), "", "{outline}");
    }
    assert_eq!(
        typed(hit(&mut engine, "KAT")).trim(),
        "cats",
        "the rest of the entry still types"
    );
    assert!(ran.ran().is_empty(), "{:?}", ran.ran());
}

#[test]
fn the_tape_never_shows_a_shell_command_line() {
    let (mut engine, _) = engine(r#"{"A": "{:shell:echo secret}"}"#, true);
    let hub = engine.hub;
    hit(&mut engine, "A");
    let tape = hub.snapshot().tape;
    assert!(
        tape[0].transparent && !tape[0].text.contains("secret") && tape[0].text.contains("shell"),
        "{:?}",
        tape[0]
    );
}

#[test]
fn the_winning_file_decides_not_the_file_an_entry_was_shadowed_from() {
    let rig = Rig::new("trust-winner");
    let steno = rig.config_dir.join("steno");
    fs::create_dir_all(&steno).unwrap();
    fs::write(
        steno.join("00-user.json"),
        r#"{"A": "{:shell:from-user}", "E": "{:run:password}"}"#,
    )
    .unwrap();
    fs::write(steno.join("theory.json"), r#"{"A": "{:shell:from-theory}", "E": "{:shell:from-theory-E}", "O": "{:shell:theory-only}"}"#).unwrap();
    let run = |order: &[&str], trust: Option<Vec<String>>| -> Vec<String> {
        let recorder = Arc::new(Recorder::default());
        let mut config = rig.config();
        config.dictionaries = order
            .iter()
            .map(|name| steno.join(name).to_string_lossy().into_owned())
            .collect();
        let names = trust.unwrap_or_else(|| {
            crate::config::config::DEFAULT_TRUSTED_DICTIONARIES
                .map(String::from)
                .to_vec()
        });
        let runtime = Runtime {
            trust: Trust::new(&steno, &names),
            launcher: Launcher::new(commands(), recorder.clone()),
        };
        let history = journal::new_shared(crate::steno::formatter::Formatter::default());
        let mut engine = StenoEngine::from_config_with(
            &config,
            true,
            &rig.config_dir,
            None,
            &history,
            "t",
            runtime,
        )
        .unwrap();
        engine.wait_until_loaded();
        for outline in ["A", "E", "O"] {
            hit(&mut engine, outline);
        }
        recorder.ran()
    };
    // The user's file first: its entries win and are trusted; the theory's O has no rival and is ignored
    assert_eq!(
        run(&["00-user.json", "theory.json"], None),
        ["from-user", "/home/u/password.sh"]
    );
    // The theory first: it wins A and E and is untrusted, so the trusted entries underneath it never run
    assert_eq!(
        run(&["theory.json", "00-user.json"], None),
        Vec::<String>::new()
    );
    // Trusting the theory by name, and no longer the user's file
    let by_name = Some(vec!["theory.json".to_string()]);
    assert_eq!(
        run(&["00-user.json", "theory.json"], by_name.clone()),
        ["/home/u/password.sh", "theory-only"]
    );
    assert_eq!(
        run(&["theory.json", "00-user.json"], by_name),
        ["from-theory", "from-theory-E", "theory-only"]
    );
}

#[test]
fn a_dictionary_names_files_in_a_folder_called_user_as_trusted_by_default() {
    let rig = Rig::new("trust-folder");
    let steno = rig.config_dir.join("steno");
    fs::create_dir_all(steno.join("user")).unwrap();
    fs::create_dir_all(steno.join("lyk/user")).unwrap();
    fs::write(steno.join("user/mine.json"), r#"{"A": "{:shell:mine}"}"#).unwrap();
    fs::write(
        steno.join("lyk/user/theirs.json"),
        r#"{"E": "{:shell:theirs}"}"#,
    )
    .unwrap();
    let recorder = Arc::new(Recorder::default());
    let mut config = rig.config();
    config.dictionaries = Vec::new();
    let runtime = Runtime {
        trust: Trust::standard(&steno),
        launcher: Launcher::new(commands(), recorder.clone()),
    };
    let history = journal::new_shared(crate::steno::formatter::Formatter::default());
    let mut engine =
        StenoEngine::from_config_with(&config, true, &rig.config_dir, None, &history, "t", runtime)
            .unwrap();
    engine.wait_until_loaded();
    hit(&mut engine, "A");
    hit(&mut engine, "E");
    assert_eq!(
        recorder.ran(),
        ["mine"],
        "only a folder at the top of the steno folder is trusted"
    );
}
