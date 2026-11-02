//! Bakes the git revision into the build, for the startup banner and the management UI.
//!
//! Order:
//! 1. `RETHINK_GIT_SHA` env (Docker build-arg / CI: a Docker build has no `.git` to ask)
//! 2. `git describe --always --dirty --tags`
//! 3. literal `unknown`
//!
//! A full 40-digit commit hash, which is what CI passes, is shortened to match what
//! `git describe` prints.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=RETHINK_GIT_SHA");

    let revision = match std::env::var("RETHINK_GIT_SHA") {
        Ok(rev) if !rev.trim().is_empty() => rev.trim().to_string(),
        _ => {
            for path in git_state_paths() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
            git(&["describe", "--always", "--dirty", "--tags"])
                .unwrap_or_else(|| "unknown".to_string())
        }
    };

    println!("cargo:rustc-env=RETHINK_GIT_SHA={}", shorten(&revision));
}

fn shorten(revision: &str) -> &str {
    let full_hash = revision.len() == 40
        && revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if full_hash {
        &revision[..7]
    } else {
        revision
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// What `git describe` reads: this worktree's HEAD, index and branch, and the tags.
///
/// Asked from git because in a linked worktree `.git` is a file, not the directory.
/// Missing paths are left out, as cargo would rerun this script on every build for them.
fn git_state_paths() -> Vec<PathBuf> {
    let Some(dirs) = git(&["rev-parse", "--git-dir", "--git-common-dir"]) else {
        return Vec::new();
    };
    let mut dirs = dirs.lines().map(|line| Path::new(line.trim()));
    let (Some(git_dir), Some(common_dir)) = (dirs.next(), dirs.next()) else {
        return Vec::new();
    };
    let mut paths = vec![
        git_dir.join("HEAD"),
        git_dir.join("index"),
        common_dir.join("packed-refs"),
        common_dir.join("refs/tags"),
        common_dir.join("reftable"),
    ];
    // Detached HEAD has no branch; HEAD itself changes then.
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        paths.push(common_dir.join(branch));
    }
    paths.retain(|path| path.exists());
    paths
}
