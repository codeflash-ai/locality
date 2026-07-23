use std::sync::Arc;

use locality_confluence::{
    ConfluenceApi, ConfluenceConfig, ConfluenceConnector, ConfluencePage, ConfluencePageBody,
    ConfluenceSpace, ConfluenceVersion,
};
use locality_connector::{ChildContainer, Connector, FetchRequest, ListChildrenRequest};
use locality_core::model::{MountId, RemoteId};
use locality_core::{LocalityError, LocalityResult};

#[test]
fn confluence_connector_projects_spaces_and_pages_as_files() {
    let connector = test_connector();
    let mount_id = MountId::new("confluence-main");

    let root = connector
        .list_children(ListChildrenRequest {
            mount_id: mount_id.clone(),
            container: ChildContainer::Root,
            parent_path: "".into(),
        })
        .expect("root children");
    assert_eq!(root.entries[0].path, std::path::PathBuf::from("Spaces"));

    let spaces = connector
        .list_children(ListChildrenRequest {
            mount_id: mount_id.clone(),
            container: ChildContainer::DirectoryChildren(RemoteId::new("confluence:spaces")),
            parent_path: "Spaces".into(),
        })
        .expect("space children");
    assert_eq!(
        spaces.entries[0].path,
        std::path::PathBuf::from("Spaces/ENG Engineering")
    );

    let space_children = connector
        .list_children(ListChildrenRequest {
            mount_id: mount_id.clone(),
            container: ChildContainer::DirectoryChildren(RemoteId::new("confluence:space:100")),
            parent_path: "Spaces/ENG Engineering".into(),
        })
        .expect("space metadata children");
    let paths = space_children
        .entries
        .iter()
        .map(|entry| entry.path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "Spaces/ENG Engineering/space.md",
            "Spaces/ENG Engineering/Pages",
        ]
    );

    let page_children = connector
        .list_children(ListChildrenRequest {
            mount_id,
            container: ChildContainer::DirectoryChildren(RemoteId::new("confluence:pages:100")),
            parent_path: "Spaces/ENG Engineering/Pages".into(),
        })
        .expect("page children");
    assert_eq!(
        page_children.entries[0].path,
        std::path::PathBuf::from("Spaces/ENG Engineering/Pages/Launch plan 1001/page.md")
    );
}

#[test]
fn confluence_connector_hydrates_page_storage_markup() {
    let connector = test_connector();
    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("confluence:page:1001"),
        })
        .expect("page native");
    let document = connector.render(&native).expect("page render");

    assert!(document.frontmatter.contains("connector: confluence"));
    assert!(document.frontmatter.contains("kind: page"));
    assert!(document.frontmatter.contains("id: \"1001\""));
    assert_eq!(document.body, "<p>Ship the docs.</p>\n");
}

#[test]
fn confluence_connector_is_read_only() {
    let connector = test_connector();

    assert!(connector.supported_push_operations().is_empty());
    assert!(matches!(
        connector.parse(&locality_core::model::CanonicalDocument::new("", "")),
        Err(LocalityError::Unsupported(message)) if message == "Confluence writes"
    ));
}

fn test_connector() -> ConfluenceConnector {
    ConfluenceConnector::with_api(
        ConfluenceConfig::new(
            "https://codeflash.atlassian.net",
            "user@example.com",
            "test-token",
        ),
        Arc::new(FakeConfluenceApi::default()),
    )
}

#[derive(Clone, Debug)]
struct FakeConfluenceApi {
    space: ConfluenceSpace,
    page: ConfluencePage,
}

impl Default for FakeConfluenceApi {
    fn default() -> Self {
        Self {
            space: ConfluenceSpace {
                id: "100".to_string(),
                key: "ENG".to_string(),
                name: "Engineering".to_string(),
                r#type: "global".to_string(),
                status: "current".to_string(),
                homepage_id: Some("1001".to_string()),
                links: Default::default(),
            },
            page: ConfluencePage {
                id: "1001".to_string(),
                title: "Launch plan".to_string(),
                status: "current".to_string(),
                space_id: "100".to_string(),
                parent_id: None,
                author_id: Some("abc".to_string()),
                created_at: Some("2026-07-23T00:00:00Z".to_string()),
                version: Some(ConfluenceVersion {
                    number: 2,
                    created_at: Some("2026-07-23T01:00:00Z".to_string()),
                }),
                body: Some(ConfluencePageBody {
                    storage: Some(locality_confluence::ConfluenceBodyRepresentation {
                        value: "<p>Ship the docs.</p>".to_string(),
                        representation: "storage".to_string(),
                    }),
                    atlas_doc_format: None,
                }),
                links: Default::default(),
            },
        }
    }
}

impl ConfluenceApi for FakeConfluenceApi {
    fn list_spaces(&self) -> LocalityResult<Vec<ConfluenceSpace>> {
        Ok(vec![self.space.clone()])
    }

    fn get_space(&self, space_id: &str) -> LocalityResult<ConfluenceSpace> {
        if space_id == self.space.id {
            return Ok(self.space.clone());
        }
        Err(LocalityError::RemoteNotFound(space_id.to_string()))
    }

    fn list_pages(&self, space_id: &str) -> LocalityResult<Vec<ConfluencePage>> {
        if space_id == self.space.id {
            return Ok(vec![self.page.clone()]);
        }
        Ok(Vec::new())
    }

    fn get_page(&self, page_id: &str) -> LocalityResult<ConfluencePage> {
        if page_id == self.page.id {
            return Ok(self.page.clone());
        }
        Err(LocalityError::RemoteNotFound(page_id.to_string()))
    }
}
