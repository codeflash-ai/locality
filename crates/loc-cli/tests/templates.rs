use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::templates::{
    TemplateApplyOptions, TemplateNewOptions, TemplatePackError, run_template_apply,
    run_template_list, run_template_new, run_template_validate,
};

#[test]
fn bundled_template_packs_are_listed_without_connectors() {
    let report = run_template_list().expect("list templates");

    assert!(report.ok);
    assert!(report.packs.iter().any(|pack| {
        pack.id == "founder-proof-of-work"
            && pack.requires.connectors.is_empty()
            && pack.outputs.contains(&"pptx".to_string())
    }));
    assert!(report.packs.iter().any(|pack| pack.id == "focused-inbox"));
}

#[test]
fn bundled_template_pack_creates_local_workspace() {
    let temp = TestTempDir::new("loc-template-new");
    let target = temp.path("workspace");

    let report = run_template_new(TemplateNewOptions {
        pack: "founder-proof-of-work".to_string(),
        path: target.clone(),
        force: false,
    })
    .expect("create workspace");

    assert_eq!(report.pack.id, "founder-proof-of-work");
    assert!(report.files_written.contains(&"index.md".to_string()));
    assert!(report.files_written.contains(&"log.md".to_string()));
    assert!(
        report
            .files_written
            .contains(&"templates/yc-application.md".to_string())
    );
    assert!(target.join(".locality-pack.yaml").exists());
    assert!(target.join("policies/publish-rules.md").exists());

    let validate = run_template_validate(target).expect("validate created workspace");
    assert_eq!(validate.pack.id, "founder-proof-of-work");
    assert!(validate.issues.is_empty());
}

#[test]
fn template_new_refuses_non_empty_target_without_force() {
    let temp = TestTempDir::new("loc-template-non-empty");
    let target = temp.path("workspace");
    fs::create_dir_all(&target).expect("target");
    fs::write(target.join("existing.md"), "keep").expect("existing");

    let error = run_template_new(TemplateNewOptions {
        pack: "focused-inbox".to_string(),
        path: target.clone(),
        force: false,
    })
    .expect_err("non-empty target");

    assert!(matches!(error, TemplatePackError::TargetNotEmpty(path) if path == target));
}

#[test]
fn template_apply_writes_bundled_template_with_title() {
    let temp = TestTempDir::new("loc-template-apply");
    let target = temp.path("notion");

    let report = run_template_apply(TemplateApplyOptions {
        pack: "founder-proof-of-work".to_string(),
        template: "weekly-update".to_string(),
        target_dir: target.clone(),
        title: Some("Week 26 Update".to_string()),
        force: false,
    })
    .expect("apply template");

    let draft = target.join("Week 26 Update.md");
    assert_eq!(report.command, "templates_apply");
    assert_eq!(report.template, "templates/weekly-update.md");
    assert_eq!(report.path, draft.display().to_string());
    assert!(
        report
            .suggested_next
            .iter()
            .any(|next| next.contains("loc diff"))
    );
    let body = fs::read_to_string(draft).expect("draft body");
    assert!(body.contains("title: \"Week 26 Update\""));
    assert!(body.contains("# Weekly Update"));
}

#[test]
fn template_apply_refuses_existing_file_without_force() {
    let temp = TestTempDir::new("loc-template-apply-existing");
    let target = temp.path("notion");
    fs::create_dir_all(&target).expect("target");
    let draft = target.join("needs-reply.md");
    fs::write(&draft, "keep").expect("existing draft");

    let error = run_template_apply(TemplateApplyOptions {
        pack: "focused-inbox".to_string(),
        template: "needs-reply.md".to_string(),
        target_dir: target,
        title: None,
        force: false,
    })
    .expect_err("existing file");

    assert!(matches!(error, TemplatePackError::FileExists(path) if path == draft));
}

#[test]
fn local_template_pack_validates_and_instantiates() {
    let temp = TestTempDir::new("loc-template-local");
    let pack = temp.path("pack");
    let target = temp.path("workspace");
    fs::create_dir_all(pack.join("templates")).expect("pack dirs");
    fs::write(
        pack.join(".locality-pack.yaml"),
        r#"id: local-pack
name: Local Pack
version: 0.1.0
description: Local test pack.
requires:
  connectors: []
outputs:
  - markdown
safety:
  default_visibility: private
  requires_review: true
"#,
    )
    .expect("manifest");
    fs::write(pack.join("templates/example.md"), "# Example\n").expect("template");

    let validate = run_template_validate(pack.clone()).expect("validate local pack");
    assert_eq!(validate.pack.id, "local-pack");
    assert_eq!(validate.pack.source, "local");

    let report = run_template_new(TemplateNewOptions {
        pack: pack.display().to_string(),
        path: target.clone(),
        force: false,
    })
    .expect("instantiate local pack");

    assert_eq!(report.pack.id, "local-pack");
    assert!(target.join("templates/example.md").exists());
}

struct TestTempDir {
    root: PathBuf,
}

impl TestTempDir {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "{prefix}-{}-{nanos}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
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

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
