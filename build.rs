//! Works out the version string the binary reports (`keymux --version`).
//!
//! Priority:
//! 1. `KEYMUX_VERSION` in the environment. Release builds set this to the release version.
//! 2. `git describe` on a checkout:
//!    - exactly on a tag:          `1.10.0`
//!    - commits past a tag:        `1.10.0+8.g0587d41`
//!    - uncommitted changes:       `...+dirty` (added to the end)
//! 3. No tags or no git (e.g. an AUR tarball): `0.0.0+g<hash>`, or `0.0.0` without git.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

fn git_version() -> Option<String> {
    // `v1.10.0-8-g0587d41`
    let described = git(&["describe", "--tags", "--long", "--match", "v[0-9]*"])?;
    // `status` refreshes the index first, so a file that was edited and then restored reads as clean.
    // (`describe --dirty` can report stale results for that case.)
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    let mut parts = described.rsplitn(3, '-');
    let hash = parts.next()?;
    let count: u32 = parts.next()?.parse().ok()?;
    let tag = parts.next()?.strip_prefix('v')?;

    let mut meta = Vec::new();
    if count > 0 {
        meta.push(format!("{count}.{hash}"));
    }
    if dirty {
        meta.push("dirty".to_string());
    }
    Some(if meta.is_empty() { tag.to_string() } else { format!("{tag}+{}", meta.join(".")) })
}

fn untagged_version() -> String {
    match git(&["rev-parse", "--short", "HEAD"]) {
        Some(hash) => format!("0.0.0+g{hash}"),
        None => "0.0.0".to_string(),
    }
}

/// Re-run this script when the checked-out commit, its branch, tags, or any tracked file change
/// (`dirty` depends on the contents of tracked files, not just on commits).
fn watch_git() {
    // Git internals live under .git/, so resolve them with --git-path.
    let mut internals = vec!["HEAD".to_string(), "packed-refs".to_string(), "index".to_string(), "refs/tags".to_string()];
    if let Some(branch_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        internals.push(branch_ref);
    }
    for p in internals {
        if let Some(path) = git(&["rev-parse", "--git-path", &p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    // Tracked files are real paths in the worktree; --git-path would point into .git/ instead.
    if let Some(files) = git(&["ls-files"]) {
        for file in files.lines() {
            println!("cargo:rerun-if-changed={file}");
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=KEYMUX_VERSION");
    watch_git();

    let version = std::env::var("KEYMUX_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(git_version)
        .unwrap_or_else(untagged_version);
    println!("cargo:rustc-env=KEYMUX_VERSION={version}");
}
