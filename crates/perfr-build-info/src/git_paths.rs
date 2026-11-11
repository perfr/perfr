// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The files that change when the commit a checkout is built from does.
//!
//! `build.rs` hands them to cargo as `rerun-if-changed`, so the stamped hash
//! follows the checkout. Where they are is git's business, and guessing
//! `<root>/.git/HEAD` and the loose file of the branch was wrong twice: in a
//! linked worktree `.git` is a file and `HEAD` lives in the main checkout's
//! `worktrees/<name>/`, and a branch whose ref has been packed (`git gc`)
//! has no loose file to watch when the script runs, while `HEAD` itself
//! only says `ref: refs/heads/main` and does not change on a commit. Either
//! way cargo kept the hash of an older commit, and `perfr --version` and
//! the About window named a commit the binary was not built from.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(checkout: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").arg("-C").arg(checkout).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The files to watch for the checkout that contains `checkout`; empty
/// without git or outside a repository.
///
/// - `HEAD`: a branch switch, or a detached head moving.
/// - `logs/HEAD`, its reflog: every commit, reset, pull and rebase appends
///   a line, whether the branch's ref is loose or packed. This is the one
///   that catches an ordinary commit.
/// - The branch's loose ref and `packed-refs`: for a repository with
///   reflogs turned off, where they are all there is.
///
/// Only files that exist are returned: cargo treats a watched path that is
/// missing as always changed and would rebuild on every invocation. So with
/// reflogs off, a first commit after a `git gc` can still go unseen (it
/// creates the loose ref, which was not there to watch).
pub fn head_watch_paths(checkout: &Path) -> Vec<PathBuf> {
    let mut names = vec!["HEAD".to_string(), "logs/HEAD".to_string(), "packed-refs".to_string()];
    if let Some(branch) = git(checkout, &["symbolic-ref", "-q", "HEAD"]) {
        names.push(branch);
    }
    names
        .iter()
        // Asked of git one by one: it answers with the worktree's own
        // directory for `HEAD` and the shared one for refs.
        .filter_map(|name| git(checkout, &["rev-parse", "--git-path", name]))
        // Relative answers are relative to where git ran.
        .map(|path| checkout.join(path))
        .filter(|path| path.is_file())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=perfr test", "-c", "user.email=test@perfr.invalid", "-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} in {}", dir.display());
    }

    fn commit(dir: &Path, text: &str) {
        std::fs::write(dir.join("file"), text).unwrap();
        run(dir, &["add", "file"]);
        run(dir, &["commit", "-q", "-m", text]);
    }

    fn contents(paths: &[PathBuf]) -> Vec<Vec<u8>> {
        paths.iter().map(|path| std::fs::read(path).unwrap()).collect()
    }

    /// The two checkouts the old guess got wrong, each asked the one
    /// question that matters: does a commit change a watched file?
    #[test]
    fn a_commit_changes_a_watched_file_in_a_worktree_and_after_refs_are_packed() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipped: no git");
            return;
        }
        let base = std::env::temp_dir().join(format!("perfr-build-info-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["-c", "init.defaultBranch=main", "init", "-q"]);
        commit(&repo, "one");

        // A linked worktree: `.git` there is a file, not a directory.
        let worktree = base.join("worktree");
        run(&repo, &["worktree", "add", "-q", "-b", "side", worktree.to_str().unwrap()]);
        assert!(worktree.join(".git").is_file());
        let watched = head_watch_paths(&worktree);
        assert!(
            watched.iter().any(|path| path.ends_with("worktrees/worktree/HEAD")),
            "the worktree's own HEAD is watched: {watched:?}"
        );
        let before = contents(&watched);
        commit(&worktree, "two");
        assert_ne!(contents(&watched), before, "a commit in the worktree changed none of {watched:?}");

        // Packed refs: the branch has no loose file, HEAD does not change.
        run(&repo, &["pack-refs", "--all"]);
        assert!(!repo.join(".git/refs/heads/main").exists());
        let watched = head_watch_paths(&repo);
        assert!(watched.iter().any(|path| path.ends_with(".git/packed-refs")), "{watched:?}");
        let before = contents(&watched);
        commit(&repo, "three");
        assert_ne!(contents(&watched), before, "a commit on a packed branch changed none of {watched:?}");

        // Asked from a subdirectory, as the build script is.
        let sub = repo.join("crates/x");
        std::fs::create_dir_all(&sub).unwrap();
        let from_sub = head_watch_paths(&sub);
        assert_eq!(from_sub.len(), watched.len() + 1, "the commit made the loose ref: {from_sub:?}");
        assert!(from_sub.iter().all(|path| path.is_file()));

        assert!(head_watch_paths(&base).is_empty(), "not a repository");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
