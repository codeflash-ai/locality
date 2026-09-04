use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn stdio_mcp_starts_without_creating_an_http_token() {
    let state_root = temp_root("loc-cli-mcp-stdio");
    let mut child = Command::new(env!("CARGO_BIN_EXE_loc"))
        .arg("mcp")
        .env("LOCALITY_STATE_DIR", &state_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start loc mcp");
    let mut stdin = child.stdin.take().expect("open loc mcp stdin");
    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}
"#,
        )
        .expect("write initialize request");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for loc mcp");
    let response: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse initialize response");

    assert!(output.status.success());
    assert_eq!(response["result"]["serverInfo"]["name"], "loc");
    assert!(!state_root.join("mcp-token").exists());
    let _ = fs::remove_dir_all(state_root);
}

fn temp_root(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()))
}
