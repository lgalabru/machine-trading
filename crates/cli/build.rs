//! Records the commit `mtx` is built from, when it's built from a git
//! checkout, so `mtx bot new --git` can pin the bot SDK to it.

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    if let Some(commit) = commit {
        println!("cargo:rustc-env=MTX_GIT_COMMIT={commit}");
    }
}
