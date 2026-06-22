use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const LIVE_WORKSPACE_ROOT_ENV: &str = "AFS_LIVE_WORKSPACE_ROOT";
const LIVE_WORKSPACE_FILE_ENV: &str = "AFS_LIVE_WORKSPACE_FILE";
const DEFAULT_WORKSPACE_FILE: &str = "weekly-to-do-list/page.md";

#[test]
#[ignore = "destructive live regression for the ali-workspace Linux FUSE mount"]
fn live_workspace_pull_edit_pull_push_regression() {
    let afs = env!("CARGO_BIN_EXE_afs");
    let root = PathBuf::from(
        env::var(LIVE_WORKSPACE_ROOT_ENV)
            .unwrap_or_else(|_| panic!("set {LIVE_WORKSPACE_ROOT_ENV} to run this live test")),
    );
    let file = root.join(
        env::var(LIVE_WORKSPACE_FILE_ENV).unwrap_or_else(|_| DEFAULT_WORKSPACE_FILE.to_string()),
    );
    assert!(
        root.exists(),
        "workspace root does not exist: {}",
        root.display()
    );
    assert!(
        file.exists(),
        "workspace file does not exist: {}",
        file.display()
    );

    json_ok(command(afs, ["pull"]).arg(&root).arg("--json"));
    json_ok(command(afs, ["pull"]).arg(&file).arg("--json"));
    assert_stable_metadata(&file);

    let _restore = RestoreOnDrop::new(afs, file.clone());
    let original = fs::read_to_string(&file).expect("read mounted file");
    let marker = format!("AFS live workspace regression {}", unique_suffix());
    fs::write(
        &file,
        insert_after_frontmatter(&original, &format!("# {marker}\n\n")),
    )
    .expect("write mounted file edit");

    let edited = fs::read_to_string(&file).expect("read edited mounted file");
    assert!(edited.contains(&marker), "{edited}");

    let pull_dirty = json_allow_failure(command(afs, ["pull"]).arg(&file).arg("--json"));
    assert_eq!(pull_dirty["skipped_dirty"], 1);
    assert_eq!(
        pull_dirty["conflicts"]
            .as_array()
            .expect("conflicts array")
            .len(),
        0,
        "{pull_dirty:#?}"
    );
    assert_no_conflict_markers(&fs::read_to_string(&file).expect("read after dirty pull"));

    let pushed = json_ok(command(afs, ["push"]).arg(&file).arg("-y").arg("--json"));
    assert_eq!(pushed["ok"], true, "{pushed:#?}");

    let pulled_after_push = json_ok(command(afs, ["pull"]).arg(&file).arg("--json"));
    assert_eq!(pulled_after_push["ok"], true, "{pulled_after_push:#?}");
    let final_contents = fs::read_to_string(&file).expect("read final mounted file");
    assert!(final_contents.contains(&marker), "{final_contents}");
    assert_no_conflict_markers(&final_contents);
}

fn command<const N: usize>(afs: &str, args: [&str; N]) -> Command {
    let mut command = Command::new(afs);
    command.args(args);
    command
}

fn json_ok(command: &mut Command) -> Value {
    let output = command.output().expect("run afs command");
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_json(output)
}

fn json_allow_failure(command: &mut Command) -> Value {
    parse_json(command.output().expect("run afs command"))
}

fn parse_json(output: Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "failed to parse command JSON: {error}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_stable_metadata(path: &Path) {
    let first = fs::metadata(path).expect("first metadata");
    thread::sleep(Duration::from_millis(1200));
    let second = fs::metadata(path).expect("second metadata");

    assert_eq!(first.len(), second.len(), "file size changed without edits");
    assert_eq!(
        first.modified().expect("first modified time"),
        second.modified().expect("second modified time"),
        "file mtime changed without edits"
    );
}

fn insert_after_frontmatter(original: &str, insertion: &str) -> String {
    if let Some(rest) = original.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        let insert_at = "---\n".len() + end + "\n---\n".len();
        let mut edited = String::with_capacity(original.len() + insertion.len());
        edited.push_str(&original[..insert_at]);
        edited.push_str(insertion);
        edited.push_str(&original[insert_at..]);
        return edited;
    }

    format!("{insertion}{original}")
}

fn assert_no_conflict_markers(contents: &str) {
    assert!(!contents.contains("<<<<<<<"), "{contents}");
    assert!(!contents.contains("======="), "{contents}");
    assert!(!contents.contains(">>>>>>>"), "{contents}");
}

fn unique_suffix() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos()
        .to_string()
}

struct RestoreOnDrop<'a> {
    afs: &'a str,
    file: PathBuf,
}

impl<'a> RestoreOnDrop<'a> {
    fn new(afs: &'a str, file: PathBuf) -> Self {
        Self { afs, file }
    }
}

impl Drop for RestoreOnDrop<'_> {
    fn drop(&mut self) {
        let _ = Command::new(self.afs)
            .arg("restore")
            .arg(&self.file)
            .arg("--force")
            .arg("--json")
            .output();
    }
}
