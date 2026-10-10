//! Report the same release tag as the other staging command-line programs.
use std::process::Command;

fn main() {
    let version = Command::new("git")
        .args(["describe", "--tags", "--dirty", "--always"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|version| version.trim().to_owned())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
    println!("cargo:rustc-env=GIT_VERSION={version}");
    for path in ["HEAD", "refs/tags", "packed-refs", "logs/HEAD"] {
        println!("cargo:rerun-if-changed=../.git/{path}");
    }
}
