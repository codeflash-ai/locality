use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::okf::{OkfExportError, OkfExportOptions, run_okf_export};
use locality_core::model::CanonicalDocument;

#[test]
fn okf_export_maps_page_directories_to_concept_documents() {
    let temp = TestTempDir::new("loc-okf-export");
    let source = temp.path("notion");
    let output = temp.path("okf");
    fs::create_dir_all(source.join("engineering-wiki/standups")).expect("source dirs");
    fs::create_dir_all(source.join("engineering-wiki/stub")).expect("stub dir");
    fs::write(
        source.join("engineering-wiki/page.md"),
        r#"---
loc:
  id: page-1
  type: page
  synced_at: "2026-07-02T10:00:00Z"
  remote_edited_at: "2026-07-02T10:00:00Z"
title: Engineering Wiki
description: Engineering operating docs.
tags:
  - engineering
Status: Active
---
# Engineering Wiki

Useful context.
"#,
    )
    .expect("root page");
    fs::write(
        source.join("engineering-wiki/standups/page.md"),
        r#"---
loc:
  id: page-2
  type: page
  synced_at: "2026-07-03T10:00:00Z"
  remote_edited_at: "2026-07-03T10:00:00Z"
title: Standups
---
# Standups
"#,
    )
    .expect("child page");
    fs::write(
        source.join("engineering-wiki/stub/page.md"),
        format!(
            "---\nloc:\n  id: stub-1\n  type: page\ntitle: Stub\n---\n{}\n",
            CanonicalDocument::STUB_MARKER
        ),
    )
    .expect("stub page");
    fs::write(source.join("AGENTS.md"), "agent guidance").expect("guidance");

    let report = run_okf_export(OkfExportOptions {
        source: source.clone(),
        output: output.clone(),
        connector: Some("notion".to_string()),
    })
    .expect("okf export");

    assert_eq!(report.concepts, 2);
    assert_eq!(report.indexes, 2);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].path, "engineering-wiki/stub/page.md");
    assert_eq!(report.skipped[0].reason, "stub_not_exported");
    assert!(
        report
            .files_written
            .contains(&"engineering-wiki.md".to_string())
    );
    assert!(
        report
            .files_written
            .contains(&"engineering-wiki/standups.md".to_string())
    );
    assert!(report.files_written.contains(&"index.md".to_string()));
    assert!(
        report
            .files_written
            .contains(&"engineering-wiki/index.md".to_string())
    );

    let root_concept = fs::read_to_string(output.join("engineering-wiki.md")).expect("concept");
    assert!(root_concept.contains("type: Notion Page"));
    assert!(root_concept.contains("title: Engineering Wiki"));
    assert!(root_concept.contains("description: Engineering operating docs."));
    assert!(
        root_concept.contains("timestamp: 2026-07-02T10:00:00Z")
            || root_concept.contains("timestamp: \"2026-07-02T10:00:00Z\"")
    );
    assert!(root_concept.contains("Status: Active"));
    assert!(root_concept.contains("source_path: engineering-wiki/page.md"));
    assert!(root_concept.contains("remote_id: page-1"));
    assert!(root_concept.contains("connector: notion"));
    assert!(root_concept.contains("# Engineering Wiki"));

    let root_index = fs::read_to_string(output.join("index.md")).expect("root index");
    assert!(root_index.contains("[engineering-wiki](engineering-wiki/) - Directory"));
    assert!(root_index.contains("[Engineering Wiki](engineering-wiki.md)"));

    let child_index =
        fs::read_to_string(output.join("engineering-wiki/index.md")).expect("child index");
    assert!(child_index.contains("[Standups](standups.md)"));
}

#[test]
fn okf_export_refuses_non_empty_output_directory() {
    let temp = TestTempDir::new("loc-okf-export-non-empty");
    let source = temp.path("source");
    let output = temp.path("okf");
    fs::create_dir_all(&source).expect("source");
    fs::create_dir_all(&output).expect("output");
    fs::write(output.join("keep.md"), "keep").expect("existing");

    let error = run_okf_export(OkfExportOptions {
        source,
        output: output.clone(),
        connector: None,
    })
    .expect_err("non-empty output");

    assert!(matches!(error, OkfExportError::OutputNotEmpty(path) if path == output));
}

#[test]
fn okf_export_reports_page_directory_output_conflicts() {
    let temp = TestTempDir::new("loc-okf-export-conflict");
    let source = temp.path("source");
    let output = temp.path("okf");
    fs::create_dir_all(source.join("roadmap")).expect("source dirs");
    fs::write(
        source.join("roadmap.md"),
        "---\ntitle: Roadmap Shortcut\n---\nShortcut body.\n",
    )
    .expect("shortcut");
    fs::write(
        source.join("roadmap/page.md"),
        "---\nloc:\n  id: page-1\n  type: page\ntitle: Roadmap\n---\nPage body.\n",
    )
    .expect("page");

    let error = run_okf_export(OkfExportOptions {
        source,
        output,
        connector: None,
    })
    .expect_err("output conflict");

    assert!(
        matches!(error, OkfExportError::OutputPathConflict { path } if path == Path::new("roadmap.md"))
    );
}

struct TestTempDir {
    root: PathBuf,
}

impl TestTempDir {
    fn new(prefix: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "{prefix}-{}-{now}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("temp root");
        Self { root }
    }

    fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
