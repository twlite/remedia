use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;

    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    for key in ["REMEDIA_BUILD_VERSION", "REMEDIA_BUILD_COMMIT"] {
        println!("cargo:rerun-if-env-changed={key}");
    }

    // Track HEAD, its symbolic ref and tags so an incremental local build stays current.
    for name in ["HEAD", "packed-refs", "refs/tags"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &reference])
    {
        println!("cargo:rerun-if-changed={path}");
    }

    let version = std::env::var("REMEDIA_BUILD_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| git(&["describe", "--tags", "--match", "v[0-9]*", "--abbrev=0"]))
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap());
    let commit = std::env::var("REMEDIA_BUILD_COMMIT")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());

    println!(
        "cargo:rustc-env=REMEDIA_BUILD_VERSION={}",
        version.trim_start_matches('v')
    );
    println!("cargo:rustc-env=REMEDIA_BUILD_COMMIT={commit}");
}
