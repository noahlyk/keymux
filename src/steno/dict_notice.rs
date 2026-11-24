//! Telling the user, on the desktop, that a dictionary they just saved is wrong.
//!
//! The reload worker ([`super::live`]) calls [`Candidate::from_reload`] after reading the files
//! that changed, and each set of dictionaries keeps a [`Notices`] that decides whether it is sent
//! now: a flapping editor save must not become a stream of notifications.
//!
//! - A file that fails to parse is the loudest: it names the file and the first problem, and says
//!   whether the previous dictionary is still in use.
//! - Errors the validator finds in a file that did load (an outline the layout rejects, ...) say
//!   which entries are skipped.
//! - Warnings only make one quiet (`low` urgency) notification with the count.
//! - Clean reloads make none.
//!
//! The first load after the daemon starts reports only files that fail to parse, so a dictionary
//! with known warnings does not announce them at every login.

use super::validate::{Finding, Severity};
use std::time::{Duration, Instant};

/// The least time between two different notices for one set of dictionaries
pub const MIN_GAP: Duration = Duration::from_secs(30);

/// How long the same notice is held back after it was sent
pub const REPEAT_GAP: Duration = Duration::from_secs(600);

/// A notification to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
    /// `normal` for something broken, `low` for warnings
    pub urgency: &'static str,
}

/// What a reload found worth saying, before the rate limit looks at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate(pub Notice);

impl Candidate {
    /// The notice a reload calls for, or `None` when everything is clean (warnings aside, which
    /// make the quiet one). `problems` are the files that failed to parse (see
    /// [`super::parts::Refresh`]), `findings` what the validator found in the files that changed,
    /// `keep_old` whether the previous dictionary stays in use because of the problems.
    #[must_use]
    pub fn from_reload(problems: &[String], findings: &[Finding], keep_old: bool) -> Option<Self> {
        if let Some(first) = problems.first() {
            let more = match problems.len() - 1 {
                0 => String::new(),
                n => format!(" (and {n} more)"),
            };
            let outcome = if keep_old {
                "The previous dictionary is still in use."
            } else {
                "The file is left out."
            };
            return Some(Self(Notice {
                title: "keymux: dictionary not loaded".to_string(),
                body: format!("{first}{more}. {outcome}"),
                urgency: "normal",
            }));
        }
        let errors: Vec<&Finding> = findings
            .iter()
            .filter(|finding| finding.severity == Severity::Error)
            .collect();
        if let Some(first) = errors.first() {
            let file = std::path::Path::new(&first.file).file_name().map_or_else(
                || first.file.clone(),
                |name| name.to_string_lossy().into_owned(),
            );
            let entry = first
                .outline
                .as_ref()
                .map_or_else(String::new, |outline| format!(" {outline}:"));
            return Some(Self(Notice {
                title: "keymux: dictionary errors".to_string(),
                body: format!(
                    "{file}:{entry} {} ({} error(s); those entries are skipped). Run: keymux steno validate",
                    first.message,
                    errors.len()
                ),
                urgency: "normal",
            }));
        }
        let warnings = findings
            .iter()
            .filter(|finding| finding.severity == Severity::Warning)
            .count();
        (warnings > 0).then(|| {
            Self(Notice {
                title: "keymux: dictionary warnings".to_string(),
                body: format!("{warnings} warning(s) in the dictionaries you changed. Run: keymux steno validate"),
                urgency: "low",
            })
        })
    }
}

/// What one set of dictionaries has said lately.
#[derive(Debug, Default)]
pub struct Notices {
    last: Option<(String, Instant)>,
}

impl Notices {
    /// The notice to send now, if the rate limit lets it through: the same text is held back for
    /// [`REPEAT_GAP`], anything else for [`MIN_GAP`] after the last one. A reload with nothing to
    /// say (`None`) sends nothing and changes nothing.
    pub fn admit(&mut self, candidate: Option<Candidate>, now: Instant) -> Option<Notice> {
        let Candidate(notice) = candidate?;
        let allowed = self.last.as_ref().is_none_or(|(body, at)| {
            let gap = if *body == notice.body {
                REPEAT_GAP
            } else {
                MIN_GAP
            };
            now.duration_since(*at) >= gap
        });
        if !allowed {
            return None;
        }
        self.last = Some((notice.body.clone(), now));
        Some(notice)
    }
}

/// Show `notice` to the user who owns the dictionary files (the daemon runs as root; with
/// files owned by root there is no one to tell).
pub fn deliver(roots: &[std::path::PathBuf], notice: &Notice) {
    use std::os::unix::fs::MetadataExt;
    let owner = roots
        .iter()
        .find_map(|root| std::fs::metadata(root).ok())
        .map(|meta| meta.uid());
    let Some(uid) = owner.filter(|uid| *uid != 0) else {
        return;
    };
    #[cfg(test)]
    {
        let _ = uid;
        crate::lock(&SENT).push(notice.clone());
    }
    #[cfg(not(test))]
    super::setup::notify_user_with(uid, &notice.title, &notice.body, notice.urgency);
}

/// What tests see sent instead of a desktop notification.
#[cfg(test)]
pub(crate) static SENT: std::sync::Mutex<Vec<Notice>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
mod tests {
    use super::super::validate::Kind;
    use super::*;

    fn finding(severity: Severity) -> Finding {
        Finding {
            file: "/home/me/.config/keymux/steno/02-words.json".to_string(),
            outline: Some("KAT".to_string()),
            severity,
            kind: Kind::BadOutline,
            message: "the outline is not valid".to_string(),
            suggestion: None,
            line: None,
            column: None,
            count: None,
            layers: Vec::new(),
        }
    }

    #[test]
    fn a_clean_reload_says_nothing() {
        assert_eq!(Candidate::from_reload(&[], &[], false), None);
        // Notes are not problems
        assert_eq!(
            Candidate::from_reload(&[], &[finding(Severity::Info)], false),
            None
        );
        let mut notices = Notices::default();
        assert_eq!(notices.admit(None, Instant::now()), None);
    }

    #[test]
    fn a_file_that_does_not_parse_names_the_file_and_the_kept_dictionary() {
        let problems = vec![
            "/x/steno/a.json: expected value at line 3 column 5".to_string(),
            "/x/steno/b.json: bad".to_string(),
        ];
        let Candidate(notice) = Candidate::from_reload(&problems, &[], true).unwrap();
        assert_eq!(notice.urgency, "normal");
        assert!(
            notice
                .body
                .starts_with("/x/steno/a.json: expected value at line 3 column 5"),
            "{}",
            notice.body
        );
        assert!(notice.body.contains("(and 1 more)"), "{}", notice.body);
        assert!(
            notice.body.contains("previous dictionary is still in use"),
            "{}",
            notice.body
        );
        // The first load has no previous dictionary to keep
        let Candidate(first) = Candidate::from_reload(&problems, &[], false).unwrap();
        assert!(
            first.body.contains("left out") && !first.body.contains("previous"),
            "{}",
            first.body
        );
    }

    #[test]
    fn errors_name_the_file_the_entry_and_the_command_to_run() {
        let found = [
            finding(Severity::Warning),
            finding(Severity::Error),
            finding(Severity::Error),
        ];
        let Candidate(notice) = Candidate::from_reload(&[], &found, false).unwrap();
        assert_eq!(notice.urgency, "normal");
        assert!(
            notice
                .body
                .starts_with("02-words.json: KAT: the outline is not valid"),
            "{}",
            notice.body
        );
        assert!(
            notice.body.contains("2 error(s)") && notice.body.contains("keymux steno validate"),
            "{}",
            notice.body
        );
    }

    #[test]
    fn warnings_alone_are_one_quiet_notice_with_the_count() {
        let found = [
            finding(Severity::Warning),
            finding(Severity::Warning),
            finding(Severity::Info),
        ];
        let Candidate(notice) = Candidate::from_reload(&[], &found, false).unwrap();
        assert_eq!(notice.urgency, "low");
        assert!(
            notice.body.starts_with("2 warning(s)")
                && notice.body.contains("keymux steno validate"),
            "{}",
            notice.body
        );
    }

    #[test]
    fn a_flapping_save_is_not_a_stream_of_notifications() {
        let now = Instant::now();
        let mut notices = Notices::default();
        let broken = |text: &str| Candidate::from_reload(&[text.to_string()], &[], true);
        assert!(
            notices.admit(broken("a.json: bad"), now).is_some(),
            "the first is sent"
        );
        // The same text again, and a clean save in between, stay quiet for the repeat gap
        assert!(notices
            .admit(broken("a.json: bad"), now + Duration::from_secs(5))
            .is_none());
        assert!(notices.admit(None, now + Duration::from_secs(6)).is_none());
        let almost = (now + REPEAT_GAP)
            .checked_sub(Duration::from_secs(1))
            .unwrap();
        assert!(notices.admit(broken("a.json: bad"), almost).is_none());
        assert!(notices
            .admit(broken("a.json: bad"), now + REPEAT_GAP)
            .is_some());
        // A different problem waits only the short gap
        let later = now + REPEAT_GAP;
        let almost = (later + MIN_GAP)
            .checked_sub(Duration::from_secs(1))
            .unwrap();
        assert!(notices.admit(broken("a.json: worse"), almost).is_none());
        assert!(notices
            .admit(broken("a.json: worse"), later + MIN_GAP)
            .is_some());
    }

    #[test]
    fn a_file_with_a_bad_outline_notifies_once_the_reload_worker_sees_it() {
        use std::os::unix::fs::MetadataExt;
        let dir = crate::test_dir::TestDir::new("keymux-notice");
        // Delivery goes to the files' owner, and nobody is told about root's files
        if std::fs::metadata(&dir).unwrap().uid() == 0 {
            return;
        }
        let file = dir.join("words.json");
        std::fs::write(&file, r#"{"KAT": "cat"}"#).unwrap();
        let updates = super::super::live::subscribe(
            vec![dir.to_path_buf()],
            dir.join("cache"),
            super::super::layout::Layout::plover(),
        );
        updates
            .recv_timeout(Duration::from_secs(10))
            .expect("the first dictionary");
        std::fs::write(&file, r#"{"KAT": "cat", "BADOUTLINE9": "x"}"#).unwrap();
        let wanted = dir.display().to_string();
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let sent = crate::lock(&SENT)
                .iter()
                .any(|n| n.body.contains("words.json") && n.body.contains("BADOUTLINE9"));
            if sent {
                break;
            }
            assert!(
                Instant::now() < end,
                "no notification for {wanted}: {:?}",
                crate::lock(&SENT)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
