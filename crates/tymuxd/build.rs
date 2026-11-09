fn main() {
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok());
    if let Some(sha) = sha {
        println!("cargo:rustc-env=TYMUXD_GIT_SHA={}", sha.trim());
    }
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
