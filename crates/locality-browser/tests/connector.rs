use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use locality_browser::{
    BrowserCaptureStore, BrowserConfig, BrowserConnector, BrowserSession, BrowserTab, BrowserWindow,
};
use locality_connector::{ChildContainer, Connector, FetchRequest, ListChildrenRequest};
use locality_core::model::{MountId, RemoteId};
use locality_core::{LocalityError, LocalityResult};

#[test]
fn browser_connector_projects_saved_sessions_as_agent_readable_files() {
    let connector = test_connector();
    let mount_id = MountId::new("browser-main");

    let root = connector
        .list_children(ListChildrenRequest {
            mount_id: mount_id.clone(),
            container: ChildContainer::Root,
            parent_path: "".into(),
        })
        .expect("root children");
    assert_eq!(root.entries[0].path, PathBuf::from("Sessions"));

    let sessions = connector
        .list_children(ListChildrenRequest {
            mount_id: mount_id.clone(),
            container: ChildContainer::DirectoryChildren(RemoteId::new("browser:sessions")),
            parent_path: "Sessions".into(),
        })
        .expect("sessions children");
    assert_eq!(
        sessions.entries[0].path,
        PathBuf::from("Sessions/2026-07-28-launch-research")
    );

    let session_children = connector
        .list_children(ListChildrenRequest {
            mount_id: mount_id.clone(),
            container: ChildContainer::DirectoryChildren(RemoteId::new("browser:session:launch")),
            parent_path: "Sessions/2026-07-28-launch-research".into(),
        })
        .expect("session children");
    let paths = session_children
        .entries
        .iter()
        .map(|entry| entry.path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "Sessions/2026-07-28-launch-research/session.md",
            "Sessions/2026-07-28-launch-research/tabs",
        ]
    );

    let tabs = connector
        .list_children(ListChildrenRequest {
            mount_id,
            container: ChildContainer::DirectoryChildren(RemoteId::new(
                "browser:session-tabs:launch",
            )),
            parent_path: "Sessions/2026-07-28-launch-research/tabs".into(),
        })
        .expect("tab children");
    assert_eq!(
        tabs.entries[0].path,
        PathBuf::from(
            "Sessions/2026-07-28-launch-research/tabs/001-locality-browser-connector/page.md"
        )
    );
}

#[test]
fn browser_connector_hydrates_tab_markdown_with_artifact_links() {
    let connector = test_connector();
    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("browser:tab:launch:tab-1"),
        })
        .expect("tab native");
    let document = connector.render(&native).expect("tab render");

    assert!(document.frontmatter.contains("connector: browser"));
    assert!(document.frontmatter.contains("kind: tab"));
    assert!(
        document
            .frontmatter
            .contains("url: \"https://www.locality.dev/blog/browser\"")
    );
    assert!(document.body.contains("# Locality Browser Connector"));
    assert!(document.body.contains("## Captured Content"));
    assert!(
        document
            .body
            .contains("Agents can read this page after the browser tab is closed.")
    );
    assert!(
        document
            .body
            .contains("Saved HTML: `/captures/html/tab-1.html`")
    );
}

#[test]
fn browser_connector_hydrates_session_summary() {
    let connector = test_connector();
    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("browser:session-summary:launch"),
        })
        .expect("session native");
    let document = connector.render(&native).expect("session render");

    assert!(document.frontmatter.contains("kind: session"));
    assert!(document.frontmatter.contains("tab_count: 1"));
    assert!(
        document
            .body
            .contains("This browser session is saved as local files")
    );
    assert!(
        document
            .body
            .contains("[Locality Browser Connector](https://www.locality.dev/blog/browser)")
    );
}

#[test]
fn browser_connector_is_read_only() {
    let connector = test_connector();

    assert!(connector.supported_push_operations().is_empty());
    assert!(matches!(
        connector.parse(&locality_core::model::CanonicalDocument::new("", "")),
        Err(LocalityError::Unsupported(message)) if message == "Browser captures are read-only"
    ));
}

#[test]
fn filesystem_store_reads_session_json_files() {
    let root = temp_capture_root();
    let sessions_dir = root.join("sessions");
    fs::create_dir_all(&sessions_dir).expect("sessions dir");
    fs::write(
        sessions_dir.join("launch.json"),
        serde_json::to_string(&sample_session()).expect("session json"),
    )
    .expect("write session");

    let connector = BrowserConnector::new(BrowserConfig::new(&root));
    let entries = connector
        .enumerate(locality_connector::EnumerateRequest {
            mount_id: MountId::new("browser-main"),
            cursor: None,
        })
        .expect("enumerate");
    assert!(entries.iter().any(|entry| {
        entry.path
            == PathBuf::from(
                "Sessions/2026-07-28-launch-research/tabs/001-locality-browser-connector/page.md",
            )
    }));
}

fn test_connector() -> BrowserConnector {
    BrowserConnector::with_store(
        BrowserConfig::new("/captures"),
        Arc::new(FakeBrowserCaptureStore {
            session: sample_session(),
        }),
    )
}

#[derive(Clone, Debug)]
struct FakeBrowserCaptureStore {
    session: BrowserSession,
}

impl BrowserCaptureStore for FakeBrowserCaptureStore {
    fn capture_root(&self) -> &std::path::Path {
        std::path::Path::new("/captures")
    }

    fn list_sessions(&self) -> LocalityResult<Vec<BrowserSession>> {
        Ok(vec![self.session.clone()])
    }
}

fn sample_session() -> BrowserSession {
    BrowserSession {
        id: "launch".to_string(),
        title: "Launch research".to_string(),
        browser: Some("Chrome".to_string()),
        profile: Some("Default".to_string()),
        captured_at: Some("2026-07-28T09:00:00Z".to_string()),
        source: Some("extension".to_string()),
        windows: vec![BrowserWindow {
            id: "window-1".to_string(),
            title: Some("Research".to_string()),
            tabs: vec![BrowserTab {
                id: "tab-1".to_string(),
                title: "Locality Browser Connector".to_string(),
                url: "https://www.locality.dev/blog/browser".to_string(),
                captured_at: Some("2026-07-28T09:01:00Z".to_string()),
                status: Some("captured".to_string()),
                discarded: false,
                markdown: Some(
                    "Agents can read this page after the browser tab is closed.".to_string(),
                ),
                text: None,
                selected_text: Some("Browser memory for agents".to_string()),
                notes: Some("Use this as launch-page context.".to_string()),
                html_path: Some("html/tab-1.html".to_string()),
                screenshot_path: Some("screens/tab-1.png".to_string()),
                favicon_url: Some("https://www.locality.dev/favicon.ico".to_string()),
            }],
        }],
    }
}

fn temp_capture_root() -> PathBuf {
    let mut path = std::env::temp_dir();
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("locality-browser-test-{suffix}"));
    path
}
