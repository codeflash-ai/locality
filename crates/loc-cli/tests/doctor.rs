use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::doctor::{DoctorOptions, DoctorSeverity, DoctorStatus, doctor_exit_code, run_doctor};
use locality_core::model::{MountId, RemoteId};
use locality_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, MountConfig, MountRepository,
    ProjectionMode, SqliteStateStore,
};

#[test]
fn doctor_reports_missing_state_without_creating_sqlite() {
    let root = temp_root("loc-cli-doctor-empty");

    let report = run_doctor(DoctorOptions {
        state_root: Some(root.clone()),
    });

    assert!(report.ok);
    assert_eq!(report.status, DoctorStatus::Ok);
    assert!(!report.state_store.exists);
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.code == "state_store_missing"
                && finding.severity == DoctorSeverity::Info)
    );
    assert!(
        !root.join("state.sqlite3").exists(),
        "read-only doctor must not initialize SQLite state"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn doctor_reports_mount_and_connection_findings() {
    let state_root = temp_root("loc-cli-doctor-state");
    let missing_mount_root = temp_root("loc-cli-doctor-missing-mount");
    let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                missing_mount_root.clone(),
            )
            .with_remote_root_id(RemoteId::new("page-root"))
            .with_connection_id(ConnectionId::new("work"))
            .projection(ProjectionMode::PlainFiles),
        )
        .expect("save mount");
    store
        .save_connection(ConnectionRecord {
            connection_id: ConnectionId::new("work"),
            profile_id: None,
            connector: "notion".to_string(),
            display_name: "Work".to_string(),
            account_label: Some("agent@example.com".to_string()),
            workspace_id: Some("workspace-1".to_string()),
            workspace_name: Some("Workspace".to_string()),
            auth_kind: "token".to_string(),
            secret_ref: "connection:work".to_string(),
            scopes: Vec::new(),
            capabilities_json: "{}".to_string(),
            status: "revoked".to_string(),
            created_at: "2026-06-20T00:00:00Z".to_string(),
            updated_at: "2026-06-20T00:00:00Z".to_string(),
            expires_at: None,
        })
        .expect("save connection");
    drop(store);

    let report = run_doctor(DoctorOptions {
        state_root: Some(state_root.clone()),
    });

    assert!(!report.ok);
    assert_eq!(report.status, DoctorStatus::Error);
    assert_eq!(doctor_exit_code(&report), 3);
    assert!(has_finding(&report, "mount_root_missing"));
    assert!(has_finding(&report, "connection_not_active"));
    assert!(has_finding(&report, "connection_profile_missing"));
    assert!(has_finding(&report, "connection_credential_missing"));
    assert!(
        report
            .suggested_commands
            .iter()
            .any(|command| command == "loc connect notion")
    );

    let _ = fs::remove_dir_all(state_root);
    let _ = fs::remove_dir_all(missing_mount_root);
}

#[test]
fn doctor_reports_virtual_projection_root_and_mount_point_separately() {
    let state_root = temp_root("loc-cli-doctor-virtual-state");
    let shared_root = temp_root("loc-cli-doctor-shared-root");
    fs::create_dir_all(&shared_root).expect("shared root");
    let mount_point_root = shared_root.join("notion-main");
    let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
    store
        .save_mount(
            MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                mount_point_root.clone(),
            )
            .with_remote_root_id(RemoteId::new("page-root"))
            .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save virtual mount");
    drop(store);

    let report = run_doctor(DoctorOptions {
        state_root: Some(state_root.clone()),
    });

    let mount = report
        .mounts
        .iter()
        .find(|mount| mount.mount_id == "notion-main")
        .expect("doctor mount");
    let expected_mount_point = mount_point_root.display().to_string();
    assert_eq!(mount.root, shared_root.display().to_string());
    assert_eq!(
        mount.mount_point.as_deref(),
        Some(expected_mount_point.as_str())
    );
    assert!(mount.root_exists);
    assert!(!has_finding(&report, "mount_root_missing"));

    let _ = fs::remove_dir_all(state_root);
    let _ = fs::remove_dir_all(shared_root);
}

fn has_finding(report: &loc_cli::doctor::DoctorReport, code: &str) -> bool {
    report.findings.iter().any(|finding| finding.code == code)
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
