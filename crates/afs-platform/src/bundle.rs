use std::path::{Path, PathBuf};

pub fn executable_filename(name: &str) -> String {
    executable_filename_for_target(name, std::env::consts::OS)
}

pub fn executable_filename_for_target(name: &str, target_os: &str) -> String {
    if target_os == "windows" && !name.ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

pub fn bundled_binary_next_to_current_exe(name: &str) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    find_bundled_binary(executable.parent()?, name)
}

pub fn find_bundled_binary(bundle_dir: &Path, name: &str) -> Option<PathBuf> {
    bundled_binary_candidates(bundle_dir, name)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

pub fn bundled_binary_candidates(bundle_dir: &Path, name: &str) -> Vec<PathBuf> {
    bundled_binary_candidates_for_target(
        bundle_dir,
        name,
        std::env::consts::OS,
        option_env!("TAURI_ENV_TARGET_TRIPLE"),
    )
}

pub fn bundled_binary_candidates_for_target(
    bundle_dir: &Path,
    name: &str,
    target_os: &str,
    target_triple: Option<&str>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    push_unique(
        &mut candidates,
        bundle_dir.join(executable_filename_for_target(name, target_os)),
    );

    if let Some(target_triple) = target_triple.filter(|value| !value.is_empty()) {
        let suffix = if target_os == "windows" { ".exe" } else { "" };
        push_unique(
            &mut candidates,
            bundle_dir.join(format!("{name}-{target_triple}{suffix}")),
        );
    }

    candidates
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::{bundled_binary_candidates_for_target, executable_filename_for_target};
    use std::path::PathBuf;

    #[test]
    fn windows_executables_use_exe_suffix() {
        assert_eq!(executable_filename_for_target("afs", "windows"), "afs.exe");
        assert_eq!(
            executable_filename_for_target("afs.exe", "windows"),
            "afs.exe"
        );
    }

    #[test]
    fn unix_executables_preserve_name() {
        assert_eq!(executable_filename_for_target("afs", "macos"), "afs");
        assert_eq!(executable_filename_for_target("afsd", "linux"), "afsd");
    }

    #[test]
    fn sidecar_candidates_include_plain_and_tauri_triple_names() {
        assert_eq!(
            bundled_binary_candidates_for_target(
                &PathBuf::from(r"C:\Program Files\AFS"),
                "afsd",
                "windows",
                Some("x86_64-pc-windows-msvc")
            ),
            vec![
                PathBuf::from(r"C:\Program Files\AFS\afsd.exe"),
                PathBuf::from(r"C:\Program Files\AFS\afsd-x86_64-pc-windows-msvc.exe"),
            ]
        );
    }
}
