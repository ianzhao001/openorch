use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Re-run when this worktree's HEAD or checked-out branch advances. All
    // discovery is best-effort: a source archive or missing git must still
    // build, in which case the runtime stamp remains absent.
    println!("cargo:rerun-if-changed=build.rs");
    let manifest_dir = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(path) => PathBuf::from(path),
        None => return,
    };
    emit_git_rerun_paths(&manifest_dir);

    let sha = git_stdout(&manifest_dir, &["rev-parse", "HEAD"])
        .filter(|sha| is_full_git_sha(sha))
        .unwrap_or_default();
    // Always emit the key so an ambient environment variable cannot masquerade
    // as a build-script stamp when git discovery failed.
    println!("cargo:rustc-env=ORCH_BUILD_GIT_SHA={sha}");
}

fn emit_git_rerun_paths(manifest_dir: &Path) {
    if let Some(head_path) = git_stdout(manifest_dir, &["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head_path}");
    }
    if let Some(reference) = git_stdout(manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(reference_path) =
            git_stdout(manifest_dir, &["rev-parse", "--git-path", &reference])
        {
            println!("cargo:rerun-if-changed={reference_path}");
        }
    }
    if let Some(packed_refs) = git_stdout(manifest_dir, &["rev-parse", "--git-path", "packed-refs"])
    {
        println!("cargo:rerun-if-changed={packed_refs}");
    }
}

fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn is_full_git_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
