//! A stamp of the engine's source in a checkout, std only so `build.rs` can
//! share it: `mtx` is built with the stamp of the tree it was built from,
//! and `mtx task fingerprint` and `sync` compare it with the checkout's own,
//! since the family parameters and the grader's version in a fingerprint
//! come from the running binary. A stale installed `mtx` would write a
//! wrong value.
#![allow(dead_code)]

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

/// What the fingerprint takes from the binary: the engine's source.
const PATHS: &[&str] = &["crates", "Cargo.lock", "rust-toolchain"];

fn git(root: &Path, args: &[&str], stdin: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(bytes).ok()?;
    }
    let out = child.wait_with_output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// The engine source's identity: the committed trees of [`PATHS`], and a
/// hash of the edits to them and of the files not yet added. `None` outside
/// a git checkout. The second value is true when there are edits.
pub fn of(root: &Path) -> Option<(String, bool)> {
    let mut trees = String::new();
    for path in PATHS {
        let out = git(root, &["rev-parse", &format!("HEAD:{path}")], None)?;
        trees.push_str(String::from_utf8_lossy(&out).trim());
        trees.push(' ');
    }
    let mut diff_args = vec!["diff", "HEAD", "--no-ext-diff", "--no-color", "--"];
    diff_args.extend(PATHS);
    let mut edits = git(root, &diff_args, None)?;
    let mut new_args = vec!["ls-files", "--others", "--exclude-standard", "--"];
    new_args.extend(PATHS);
    edits.extend(git(root, &new_args, None)?);
    let dirty = !edits.is_empty();
    let edits_hash = if dirty {
        String::from_utf8_lossy(&git(root, &["hash-object", "--stdin"], Some(&edits))?)
            .trim()
            .to_string()
    } else {
        String::new()
    };
    Some((format!("{}{edits_hash}", trees.trim_end()), dirty))
}

/// The commit `root` is at, short, with `+dirty` if its engine source has
/// edits.
pub fn engine_label(root: &Path) -> Option<String> {
    let commit = git(root, &["rev-parse", "--short=12", "HEAD"], None)?;
    let commit = String::from_utf8_lossy(&commit).trim().to_string();
    let (stamp, dirty) = of(root)?;
    Some(if dirty {
        // The edits' hash ends the stamp: two dirty trees differ.
        let edits = stamp.get(stamp.len().saturating_sub(7)..).unwrap_or("");
        format!("{commit}+dirty.{edits}")
    } else {
        commit
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn the_stamp_follows_the_engines_source_and_nothing_else() {
        let root =
            std::env::temp_dir().join(format!("mtx-stamp-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(root.join("crates/a")).unwrap();
        for (path, text) in [
            ("crates/a/lib.rs", "fn a() {}\n"),
            ("Cargo.lock", "lock\n"),
            ("rust-toolchain", "1\n"),
            ("README.md", "r\n"),
        ] {
            std::fs::write(root.join(path), text).unwrap();
        }
        run(&root, &["init", "-q"]);
        run(&root, &["add", "-A"]);
        run(&root, &["commit", "-q", "-m", "one"]);
        let (clean, dirty) = of(&root).unwrap();
        assert!(!dirty);
        // Other files, and commits that leave the engine alone, don't move it.
        std::fs::write(root.join("README.md"), "changed\n").unwrap();
        assert_eq!(of(&root).unwrap(), (clean.clone(), false));
        run(&root, &["commit", "-q", "-am", "docs"]);
        assert_eq!(of(&root).unwrap(), (clean.clone(), false));
        // An edit, committed or not, and a new file, do.
        std::fs::write(root.join("crates/a/lib.rs"), "fn a() { 1; }\n").unwrap();
        let (edited, dirty) = of(&root).unwrap();
        assert!(dirty);
        assert_ne!(edited, clean);
        run(&root, &["commit", "-q", "-am", "engine"]);
        let (committed, dirty) = of(&root).unwrap();
        assert!(!dirty);
        assert_ne!(committed, clean);
        std::fs::write(root.join("crates/a/new.rs"), "x\n").unwrap();
        assert!(of(&root).unwrap().1);
        assert!(engine_label(&root).unwrap().contains("+dirty."));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
