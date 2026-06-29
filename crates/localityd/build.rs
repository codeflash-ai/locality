use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=LOCALITY_BUILD_ID_OVERRIDE");
    println!("cargo:rerun-if-changed=build.rs");
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let workspace = manifest_dir.join("../..");
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join(".git/HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join(".git/index").display()
    );

    let build_id = env::var("LOCALITY_BUILD_ID_OVERRIDE").unwrap_or_else(|_| git_build_id());
    println!("cargo:rustc-env=LOCALITY_BUILD_ID={build_id}");
}

fn git_build_id() -> String {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let workspace = manifest_dir.join("../..");
    let head = git_output(&workspace, &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());
    if git_is_dirty(&workspace) {
        format!("{head}-dirty")
    } else {
        head
    }
}

fn git_output(workspace: &PathBuf, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_is_dirty(workspace: &PathBuf) -> bool {
    git_output(
        workspace,
        &["status", "--porcelain", "--untracked-files=no"],
    )
    .is_some_and(|status| !status.is_empty())
}
