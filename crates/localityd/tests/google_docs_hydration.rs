use std::sync::Arc;

use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{HydrationState, MountId, RemoteId};
use locality_core::{LocalityError, LocalityResult};
use locality_google_docs::client::{GoogleDocsApi, GoogleDriveApi};
use locality_google_docs::docs_dto::{BatchUpdateDocumentRequest, GoogleDocument};
use locality_google_docs::drive_dto::{
    DRIVE_GOOGLE_DOC_MIME_TYPE, DriveComment, DriveCommentContent, DriveCommentList,
    DriveCommentUser, DriveCreateFileRequest, DriveFile, DriveFileList, DriveUpdateFileRequest,
};
use locality_google_docs::{GoogleDocsConfig, GoogleDocsConnector};
use localityd::hydration::HydrationSource;

#[test]
fn google_docs_hydration_emits_comments_sidecar_asset() {
    let connector = GoogleDocsConnector::with_apis(
        GoogleDocsConfig::new("token"),
        Arc::new(FakeDrive),
        Arc::new(FakeDocs),
    );
    let request = HydrationRequest::new(
        MountId::new("google-docs-main"),
        RemoteId::new("doc-1"),
        "launch/page.md",
        HydrationState::Stub,
        HydrationReason::ExplicitPull,
    );

    let rendered = connector
        .fetch_render(&request)
        .expect("hydrate google doc");

    assert_eq!(rendered.document.body, "Hello doc\n");
    assert_eq!(rendered.assets.len(), 1);
    assert_eq!(
        rendered.assets[0].path,
        std::path::Path::new("launch/.comments.md")
    );
    assert!(rendered.assets[0].media.is_none());
    let comments = String::from_utf8(rendered.assets[0].bytes.clone()).expect("comments utf8");
    assert_eq!(
        comments,
        concat!(
            "# Comments\n\n",
            "## Anchored comments\n\n",
            "### comment-1\n\n",
            "> Hello doc\n\n",
            "- Ada, created 2026-06-11T10:00:00.000Z\n",
            "  - anchor: kix.anchor\n",
            "  - comment: comment-1\n",
            "  - Looks good.\n"
        )
    );
}

#[derive(Debug)]
struct FakeDrive;

impl GoogleDriveApi for FakeDrive {
    fn get_file(&self, file_id: &str) -> LocalityResult<DriveFile> {
        if file_id != "doc-1" {
            return Err(LocalityError::RemoteNotFound(file_id.to_string()));
        }
        Ok(DriveFile {
            id: "doc-1".to_string(),
            name: "Launch Brief".to_string(),
            mime_type: DRIVE_GOOGLE_DOC_MIME_TYPE.to_string(),
            parents: vec!["workspace".to_string()],
            modified_time: Some("2026-06-25T10:00:00.000Z".to_string()),
            version: Some("7".to_string()),
            trashed: false,
        })
    }

    fn list_children(
        &self,
        _parent_id: &str,
        _page_token: Option<&str>,
    ) -> LocalityResult<DriveFileList> {
        Ok(DriveFileList::default())
    }

    fn list_workspace_folders_by_name(
        &self,
        _name: &str,
        _page_token: Option<&str>,
    ) -> LocalityResult<DriveFileList> {
        Ok(DriveFileList::default())
    }

    fn list_comments(
        &self,
        file_id: &str,
        _page_token: Option<&str>,
    ) -> LocalityResult<DriveCommentList> {
        if file_id != "doc-1" {
            return Err(LocalityError::RemoteNotFound(file_id.to_string()));
        }
        Ok(DriveCommentList {
            comments: vec![DriveComment {
                id: "comment-1".to_string(),
                anchor: Some("kix.anchor".to_string()),
                quoted_file_content: Some(DriveCommentContent {
                    mime_type: Some("text/plain".to_string()),
                    value: Some("Hello doc".to_string()),
                }),
                author: Some(DriveCommentUser {
                    display_name: Some("Ada".to_string()),
                    email_address: None,
                    me: false,
                }),
                content: Some("Looks good.".to_string()),
                created_time: Some("2026-06-11T10:00:00.000Z".to_string()),
                ..DriveComment::default()
            }],
            next_page_token: None,
        })
    }

    fn create_file(&self, _request: DriveCreateFileRequest) -> LocalityResult<DriveFile> {
        Err(LocalityError::Unsupported("test fake create"))
    }

    fn update_file(
        &self,
        _file_id: &str,
        _request: DriveUpdateFileRequest,
    ) -> LocalityResult<DriveFile> {
        Err(LocalityError::Unsupported("test fake update"))
    }
}

#[derive(Debug)]
struct FakeDocs;

impl GoogleDocsApi for FakeDocs {
    fn get_document(&self, document_id: &str) -> LocalityResult<GoogleDocument> {
        if document_id != "doc-1" {
            return Err(LocalityError::RemoteNotFound(document_id.to_string()));
        }
        serde_json::from_value(serde_json::json!({
            "documentId": "doc-1",
            "title": "Launch Brief",
            "revisionId": "rev-1",
            "body": {
                "content": [
                    { "startIndex": 1, "endIndex": 11, "paragraph": {
                        "elements": [{ "textRun": { "content": "Hello doc\n" } }]
                    }}
                ]
            }
        }))
        .map_err(|error| LocalityError::Io(error.to_string()))
    }

    fn batch_update_document(
        &self,
        _document_id: &str,
        _request: BatchUpdateDocumentRequest,
    ) -> LocalityResult<GoogleDocument> {
        Err(LocalityError::Unsupported("test fake batch update"))
    }
}
