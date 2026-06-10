use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::diff::{DiffError, run_diff};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_core::shadow::ShadowDocument;
use afs_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ShadowRepository, SqliteStateStore, StoreError,
};

#[test]
fn diff_reports_noop_plan() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nSame paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");

    assert!(report.ok);
    assert_eq!(report.action, "noop");
    assert!(report.validation.is_empty());
    assert_eq!(report.mount_id, "notion-main");
    assert_eq!(report.entity_id, "page-1");
    assert_eq!(report.plan.unwrap().operations.len(), 0);
}

#[test]
fn diff_reports_safe_plan_as_confirmation_needed() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");
    let plan = report.plan.expect("plan");

    assert!(report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(report.guardrail.decision, "proceed");
    assert_eq!(plan.summary.blocks_updated, 1);
    assert_eq!(plan.operations[0].operation_type(), "update_block");
}

#[test]
fn diff_surfaces_validation_issues_without_plan() {
    let fixture = DiffFixture::new();
    let mut store = fixture.store();
    let path = fixture.write_raw("Roadmap.md", "---\ntitle: Missing AFS\n---\n# Roadmap\n");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nSame paragraph."))
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert!(report.plan.is_none());
    assert_eq!(report.validation[0].code, "frontmatter_missing_afs");
    assert_eq!(report.completed_stages, vec!["parse_and_validate"]);
}

#[test]
fn diff_rejects_frontmatter_id_mismatch_before_planning() {
    let fixture = DiffFixture::new();
    let store = fixture.store();
    let path = fixture.write_page_with_id("Roadmap.md", "page-2", "# Roadmap\n\nSame paragraph.");

    let report = run_diff(&store, &path).expect("diff report");

    assert!(!report.ok);
    assert_eq!(report.action, "fix_validation");
    assert!(report.plan.is_none());
    assert_eq!(report.validation[0].code, "frontmatter_remote_id_mismatch");
}

#[test]
fn diff_returns_structured_missing_shadow_error() {
    let fixture = DiffFixture::new();
    let store = fixture.store();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nSame paragraph.");

    let error = run_diff(&store, &path).expect_err("missing shadow");

    assert_eq!(error.code(), "shadow_missing");
    assert_eq!(
        error,
        DiffError::Store(StoreError::ShadowMissing {
            mount_id: fixture.mount_id.clone(),
            entity_id: RemoteId::new("page-1"),
        })
    );
}

#[test]
fn diff_returns_structured_mount_lookup_error() {
    let fixture = DiffFixture::new();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nSame paragraph.");
    let store = InMemoryStateStore::new();

    let error = run_diff(&store, &path).expect_err("missing mount");

    assert_eq!(error.code(), "mount_not_found");
}

#[test]
fn diff_runner_works_with_sqlite_state_store() {
    let fixture = DiffFixture::new();
    let path = fixture.write_page("Roadmap.md", "# Roadmap\n\nChanged paragraph.");
    let mut store = SqliteStateStore::open(fixture.root.join(".state")).expect("open sqlite");
    store
        .save_mount(MountConfig::new(
            fixture.mount_id.clone(),
            "notion",
            fixture.root.clone(),
        ))
        .expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save entity");
    store
        .save_shadow(&fixture.mount_id, shadow("# Roadmap\n\nOld paragraph."))
        .expect("save shadow");

    let report = run_diff(&store, &path).expect("diff report");

    assert!(report.ok);
    assert_eq!(report.action, "confirm_plan");
    assert_eq!(report.plan.unwrap().summary.blocks_updated, 1);
}

struct DiffFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl DiffFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "afs-cli-diff-{}-{unique}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root");
        Self {
            root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(MountConfig::new(
                self.mount_id.clone(),
                "notion",
                self.root.clone(),
            ))
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("page-1"),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save entity");
        store
    }

    fn write_page(&self, relative_path: &str, body: &str) -> PathBuf {
        self.write_page_with_id(relative_path, "page-1", body)
    }

    fn write_page_with_id(&self, relative_path: &str, remote_id: &str, body: &str) -> PathBuf {
        self.write_raw(relative_path, &canonical_markdown(remote_id, body))
    }

    fn write_raw(&self, relative_path: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(&path, contents).expect("fixture file");
        path
    }
}

impl Drop for DiffFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn canonical_markdown(remote_id: &str, body: &str) -> String {
    format!(
        "---\nafs:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\n{body}"
    )
}

fn shadow(body: &str) -> ShadowDocument {
    ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        9,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
}

trait OperationOutputExt {
    fn operation_type(&self) -> &'static str;
}

impl OperationOutputExt for afs_cli::diff::PushOperationOutput {
    fn operation_type(&self) -> &'static str {
        match self {
            Self::UpdateBlock { .. } => "update_block",
            Self::AppendBlock { .. } => "append_block",
            Self::MoveBlock { .. } => "move_block",
            Self::ArchiveBlock { .. } => "archive_block",
            Self::ArchiveEntity { .. } => "archive_entity",
            Self::UpdateProperties { .. } => "update_properties",
            Self::CreateEntity { .. } => "create_entity",
        }
    }
}
