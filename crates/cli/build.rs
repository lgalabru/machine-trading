//! Records the commit `mtx` is built from, when it's built from a git
//! checkout, so `mtx bot new --git` can pin the bot SDK to it, and the
//! engine's source stamp, so `mtx task fingerprint` and `sync` can refuse a
//! binary built from other source than the checkout (see `source_stamp`).

#[path = "src/source_stamp.rs"]
mod source_stamp;

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let git_path = |what: &str| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "--git-path", what])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    for what in ["HEAD", "refs/heads", "index"] {
        match git_path(what) {
            // Relative to the checkout's root, not this crate's.
            Some(path) => println!("cargo:rerun-if-changed={}", root.join(path).display()),
            None => println!("cargo:rerun-if-changed=../../.git/{what}"),
        }
    }
    for what in ["crates", "Cargo.lock", "rust-toolchain"] {
        println!("cargo:rerun-if-changed=../../{what}");
    }
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    if let Some(commit) = commit {
        println!("cargo:rustc-env=MTX_GIT_COMMIT={commit}");
    }
    if let Some((stamp, _)) = source_stamp::of(&root) {
        println!("cargo:rustc-env=MTX_SOURCE_STAMP={stamp}");
    }
    if let Some(label) = source_stamp::engine_label(&root) {
        println!("cargo:rustc-env=MTX_ENGINE_STAMP={label}");
    }
}
