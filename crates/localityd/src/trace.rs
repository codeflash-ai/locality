use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value, json};

const TRACE_FILE_ENV: &str = "LOCALITY_TRACE_FILE";
const TRACE_RUN_ID_ENV: &str = "LOCALITY_TRACE_RUN_ID";

#[derive(Debug)]
pub struct TraceSpan {
    name: &'static str,
    start_ms: u128,
    started_at: Instant,
    attrs: Map<String, Value>,
    status: &'static str,
    finished: bool,
}

impl TraceSpan {
    pub fn start(name: &'static str) -> Self {
        Self {
            name,
            start_ms: unix_epoch_ms(),
            started_at: Instant::now(),
            attrs: Map::new(),
            status: "ok",
            finished: false,
        }
    }

    pub fn attr(&mut self, key: impl Into<String>, value: impl Serialize) {
        let value = serde_json::to_value(value)
            .unwrap_or_else(|_| Value::String("<unserializable>".to_string()));
        self.attrs.insert(key.into(), value);
    }

    pub fn status(&mut self, status: &'static str) {
        self.status = status;
    }

    pub fn finish(mut self) {
        self.write();
        self.finished = true;
    }

    fn write(&mut self) {
        if self.finished {
            return;
        }
        let Some(path) = trace_file() else {
            return;
        };

        let end_ms = unix_epoch_ms();
        let duration_ms = self.started_at.elapsed().as_millis();
        let mut event = json!({
            "ts_start_ms": self.start_ms,
            "ts_end_ms": end_ms,
            "duration_ms": duration_ms,
            "span": self.name,
            "status": self.status,
            "attrs": self.attrs,
        });
        if let Ok(run_id) = env::var(TRACE_RUN_ID_ENV)
            && !run_id.trim().is_empty()
        {
            event["run_id"] = Value::String(run_id);
        }

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{event}");
        }
    }
}

impl Drop for TraceSpan {
    fn drop(&mut self) {
        self.write();
        self.finished = true;
    }
}

pub fn result<T, E, F>(name: &'static str, f: F) -> Result<T, E>
where
    F: FnOnce(&mut TraceSpan) -> Result<T, E>,
{
    let mut span = TraceSpan::start(name);
    let result = f(&mut span);
    if result.is_err() {
        span.status("error");
    }
    span.finish();
    result
}

pub fn enabled() -> bool {
    trace_file().is_some()
}

fn trace_file() -> Option<PathBuf> {
    env::var_os(TRACE_FILE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn unix_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn scoped_env<T>(key: &str, value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let previous = env::var_os(key);
        match value {
            Some(value) => unsafe { env::set_var(key, value) },
            None => unsafe { env::remove_var(key) },
        }
        let result = f();
        match previous {
            Some(value) => unsafe { env::set_var(key, value) },
            None => unsafe { env::remove_var(key) },
        }
        result
    }

    #[test]
    fn disabled_trace_does_not_create_file() {
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let path = env::temp_dir().join(format!(
            "locality-disabled-trace-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        scoped_env(TRACE_FILE_ENV, None, || {
            let mut span = TraceSpan::start("test.disabled");
            span.attr("value", 1);
            span.finish();
        });

        assert!(!path.exists());
    }

    #[test]
    fn writes_trace_span_jsonl() {
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let path = env::temp_dir().join(format!(
            "locality-enabled-trace-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let path_string = path.display().to_string();

        scoped_env(TRACE_FILE_ENV, Some(&path_string), || {
            scoped_env(TRACE_RUN_ID_ENV, Some("run-1"), || {
                let mut span = TraceSpan::start("test.enabled");
                span.attr("mount_id", "notion-main");
                span.attr("count", 3);
                span.finish();
            });
        });

        let contents = std::fs::read_to_string(&path).expect("trace file should exist");
        let value: Value = serde_json::from_str(contents.trim()).expect("valid trace json");
        assert_eq!(value["span"], "test.enabled");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["run_id"], "run-1");
        assert_eq!(value["attrs"]["mount_id"], "notion-main");
        assert_eq!(value["attrs"]["count"], 3);
        assert!(value["duration_ms"].as_u64().is_some());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn result_marks_error_status() {
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let path =
            env::temp_dir().join(format!("locality-error-trace-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_string = path.display().to_string();

        scoped_env(TRACE_FILE_ENV, Some(&path_string), || {
            let _: Result<(), ()> = result("test.error", |span| {
                span.attr("phase", "fail");
                Err(())
            });
        });

        let contents = std::fs::read_to_string(&path).expect("trace file should exist");
        let value: Value = serde_json::from_str(contents.trim()).expect("valid trace json");
        assert_eq!(value["span"], "test.error");
        assert_eq!(value["status"], "error");
        assert_eq!(value["attrs"]["phase"], "fail");
        let _ = std::fs::remove_file(path);
    }
}
