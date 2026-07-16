use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=LOCALITY_BUILD_ID_OVERRIDE");
    println!("cargo:rerun-if-env-changed=LOCALITY_DESKTOP_BUILD_ID_OVERRIDE");
    println!("cargo:rerun-if-env-changed=LOCALITY_DISTRIBUTION_CHANNEL");
    println!("cargo:rerun-if-changed=build.rs");
    compile_macos_file_provider_protocol();
    let workspace = workspace_root();
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join(".git/HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join(".git/index").display()
    );
    let build_id = env::var("LOCALITY_DESKTOP_BUILD_ID_OVERRIDE")
        .or_else(|_| env::var("LOCALITY_BUILD_ID_OVERRIDE"))
        .unwrap_or_else(|_| git_build_id(&workspace));
    println!("cargo:rustc-env=LOCALITY_DESKTOP_BUILD_ID={build_id}");
    let distribution_channel =
        env::var("LOCALITY_DISTRIBUTION_CHANNEL").unwrap_or_else(|_| "direct".to_string());
    println!("cargo:rustc-env=LOCALITY_DISTRIBUTION_CHANNEL={distribution_channel}");

    tauri_build::build();
}

fn compile_macos_file_provider_protocol() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    println!("cargo:rerun-if-changed=macos/LocalityFileProviderServiceProtocol.h");
    println!("cargo:rerun-if-changed=macos/LocalityFileProviderServiceProtocol.m");

    cc::Build::new()
        .file("macos/LocalityFileProviderServiceProtocol.m")
        .compile("LocalityFileProviderServiceProtocol");
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("../../..")
}

fn git_build_id(workspace: &PathBuf) -> String {
    let head = git_output(workspace, &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());
    if git_is_dirty(workspace) {
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
