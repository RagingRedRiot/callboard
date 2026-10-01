//! Build identity (DESIGN.md §7.4): the Git commit, plus a digest of the
//! uncommitted changes (diff and untracked files) for a dirty tree, so two builds differ whenever their
//! sources do.
use std::{
    hash::{Hash, Hasher},
    path::Path,
    process::Command,
};

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // A directory is scanned recursively, so any source edit reruns this.
    for path in ["crates", "Cargo.lock", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    for name in ["HEAD", "index"] {
        if let Some(path) = git(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-path", name],
        ) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&root, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(
            &root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                &reference,
            ],
        )
    {
        println!("cargo:rerun-if-changed={path}");
    }
    let build = match git(&root, &["rev-parse", "--short=12", "HEAD"]) {
        None => "unknown".to_owned(),
        Some(commit) => {
            let diff = git(&root, &["diff", "HEAD"]);
            let untracked = git(&root, &["ls-files", "--others", "--exclude-standard"]);
            match (diff, untracked) {
                (Some(diff), Some(untracked)) if diff.is_empty() && untracked.is_empty() => commit,
                (Some(diff), Some(untracked)) => {
                    let mut hasher = std::hash::DefaultHasher::new();
                    diff.hash(&mut hasher);
                    for file in untracked.lines() {
                        file.hash(&mut hasher);
                        std::fs::read(root.join(file)).ok().hash(&mut hasher);
                    }
                    format!("{commit}-dirty.{:08x}", hasher.finish() as u32)
                }
                _ => format!("{commit}-dirty"),
            }
        }
    };
    println!("cargo:rustc-env=CALLBOARD_BUILD={build}");
}
