//! Route registration (DESIGN.md §8.1): every file in `src/routes/` becomes a
//! module whose `ROUTES` join the API table, so adding a route file never
//! edits a reviewed file.
//!
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

fn register_routes() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut modules: Vec<_> = std::fs::read_dir(&dir)
        .expect("src/routes")
        .map(|entry| entry.expect("src/routes entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "rs"))
        .collect();
    modules.sort();
    let mut source = String::new();
    let mut all = Vec::new();
    for path in &modules {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("UTF-8 name");
        assert!(
            name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "route file {} must be named like a module (lowercase, digits, _)",
            path.display()
        );
        source.push_str(&format!(
            "#[path = {:?}]\nmod {name};\n",
            path.display().to_string()
        ));
        all.push(format!("{name}::ROUTES"));
    }
    source.push_str(&format!(
        "pub(crate) const ALL: &[&[crate::api::Route]] = &[{}];\n",
        all.join(", ")
    ));
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("routes.rs");
    std::fs::write(out, source).unwrap();
}

fn main() {
    register_routes();
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
