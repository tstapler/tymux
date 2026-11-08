#![allow(clippy::disallowed_methods)] // a build script may block
use std::path::Path;
use std::process::Command;

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn watch(path: impl AsRef<Path>) {
    println!("cargo:rerun-if-changed={}", path.as_ref().display());
}

fn watch_inputs(root: &Path) {
    watch(root.join(".git"));
    for args in [
        vec!["rev-parse", "--absolute-git-dir"],
        vec!["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ] {
        if let Some(directory) = git_output(root, &args) {
            for name in ["HEAD", "index", "packed-refs", "refs"] {
                watch(Path::new(&directory).join(name));
            }
        }
    }
    if let Some(files) = git_output(root, &["ls-files"]) {
        for file in files.lines() {
            watch(root.join(file));
        }
    }
    watch("src");
    watch("build.rs");
}

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("Cargo supplies manifest directory");
    let root = Path::new(&manifest).join("../..");
    println!("cargo:rerun-if-env-changed=TYMUX_BUILD_SHA");
    println!("cargo:rerun-if-env-changed=TYMUX_BUILD_DIRTY");
    watch_inputs(&root);
    let sha = std::env::var("TYMUX_BUILD_SHA")
        .ok()
        .filter(|sha| matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .or_else(|| git_output(&root, &["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned());
    let dirty = std::env::var("TYMUX_BUILD_DIRTY").ok().unwrap_or_else(|| {
        match git_output(&root, &["status", "--porcelain", "--untracked-files=no"]) {
            Some(status) => (!status.is_empty()).to_string(),
            None => "unknown".to_owned(),
        }
    });
    println!("cargo:rustc-env=TYMUX_BUILD_SHA={sha}");
    println!("cargo:rustc-env=TYMUX_BUILD_DIRTY={dirty}");
}
