use serde::{Deserialize, Serialize};

pub const DRIVE_FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";
pub const DRIVE_GOOGLE_DOC_MIME_TYPE: &str = "application/vnd.google-apps.document";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub trashed: bool,
}

impl DriveFile {
    pub fn is_folder(&self) -> bool {
        self.mime_type == DRIVE_FOLDER_MIME_TYPE
    }

    pub fn is_google_doc(&self) -> bool {
        self.mime_type == DRIVE_GOOGLE_DOC_MIME_TYPE
    }

    pub fn remote_version(&self) -> Option<String> {
        match (&self.version, &self.modified_time) {
            (Some(version), Some(modified_time)) => {
                Some(format!("drive:{version}:{modified_time}"))
            }
            (Some(version), None) => Some(format!("drive:{version}")),
            (None, Some(modified_time)) => Some(format!("drive:{modified_time}")),
            (None, None) => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFileList {
    #[serde(default)]
    pub files: Vec<DriveFile>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveCommentList {
    #[serde(default)]
    pub comments: Vec<DriveComment>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveComment {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub anchor: Option<String>,
    #[serde(default)]
    pub quoted_file_content: Option<DriveCommentContent>,
    #[serde(default)]
    pub author: Option<DriveCommentUser>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub html_content: Option<String>,
    #[serde(default)]
    pub created_time: Option<String>,
    #[serde(default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub resolved: bool,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub replies: Vec<DriveCommentReply>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveCommentContent {
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveCommentReply {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub author: Option<DriveCommentUser>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub html_content: Option<String>,
    #[serde(default)]
    pub created_time: Option<String>,
    #[serde(default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub action: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveCommentUser {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email_address: Option<String>,
    #[serde(default)]
    pub me: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveCreateFileRequest {
    pub name: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<String>,
}

impl DriveCreateFileRequest {
    pub fn google_doc(name: impl Into<String>, parent_id: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            mime_type: DRIVE_GOOGLE_DOC_MIME_TYPE.to_string(),
            parents: vec![parent_id.into()],
        }
    }

    pub fn folder(name: impl Into<String>, parent_id: Option<&str>) -> Self {
        Self {
            name: name.into(),
            mime_type: DRIVE_FOLDER_MIME_TYPE.to_string(),
            parents: parent_id
                .map(|parent_id| vec![parent_id.to_string()])
                .unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveUpdateFileRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trashed: Option<bool>,
}

impl DriveUpdateFileRequest {
    pub fn rename(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            ..Self::default()
        }
    }

    pub fn move_to(parent_id: impl Into<String>) -> Self {
        Self {
            parents: vec![parent_id.into()],
            ..Self::default()
        }
    }

    pub fn trash() -> Self {
        Self {
            trashed: Some(true),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DRIVE_FOLDER_MIME_TYPE, DRIVE_GOOGLE_DOC_MIME_TYPE, DriveCreateFileRequest, DriveFile,
        DriveFileList, DriveUpdateFileRequest,
    };

    #[test]
    fn drive_file_list_decodes_google_docs_and_folders() {
        let payload = serde_json::json!({
            "nextPageToken": "cursor-2",
            "files": [
                {
                    "id": "folder-1",
                    "name": "Marketing",
                    "mimeType": "application/vnd.google-apps.folder",
                    "parents": ["workspace"],
                    "modifiedTime": "2026-06-25T10:00:00.000Z",
                    "version": "42",
                    "trashed": false
                },
                {
                    "id": "doc-1",
                    "name": "Launch Brief",
                    "mimeType": "application/vnd.google-apps.document",
                    "parents": ["folder-1"],
                    "modifiedTime": "2026-06-25T10:01:00.000Z",
                    "version": "43",
                    "trashed": false
                }
            ]
        });

        let decoded: DriveFileList = serde_json::from_value(payload).expect("decode list");

        assert_eq!(decoded.next_page_token.as_deref(), Some("cursor-2"));
        assert!(decoded.files[0].is_folder());
        assert!(decoded.files[1].is_google_doc());
        assert_eq!(
            decoded.files[1].remote_version(),
            Some("drive:43:2026-06-25T10:01:00.000Z".to_string())
        );
    }

    #[test]
    fn drive_create_request_builds_google_doc_under_parent() {
        let request = DriveCreateFileRequest::google_doc("Launch Brief", "folder-1");
        let json = serde_json::to_value(&request).expect("serialize create");

        assert_eq!(json["name"], "Launch Brief");
        assert_eq!(json["mimeType"], DRIVE_GOOGLE_DOC_MIME_TYPE);
        assert_eq!(json["parents"], serde_json::json!(["folder-1"]));
    }

    #[test]
    fn drive_update_request_can_rename_move_and_trash() {
        let rename = DriveUpdateFileRequest::rename("Renamed");
        let move_file = DriveUpdateFileRequest::move_to("folder-2");
        let trash = DriveUpdateFileRequest::trash();

        assert_eq!(
            serde_json::to_value(rename).expect("rename"),
            serde_json::json!({ "name": "Renamed" })
        );
        assert_eq!(
            serde_json::to_value(move_file).expect("move"),
            serde_json::json!({ "parents": ["folder-2"] })
        );
        assert_eq!(
            serde_json::to_value(trash).expect("trash"),
            serde_json::json!({ "trashed": true })
        );
    }

    #[test]
    fn drive_folder_request_uses_folder_mime_type() {
        let request = DriveCreateFileRequest::folder("Locality", None);
        let json = serde_json::to_value(&request).expect("serialize folder");

        assert_eq!(json["mimeType"], DRIVE_FOLDER_MIME_TYPE);
        assert!(json.get("parents").is_none());
    }

    #[test]
    fn drive_file_defaults_missing_collections() {
        let file: DriveFile = serde_json::from_value(serde_json::json!({
            "id": "doc-1",
            "name": "Untitled",
            "mimeType": "application/vnd.google-apps.document"
        }))
        .expect("decode minimal file");

        assert!(file.parents.is_empty());
        assert!(!file.trashed);
    }
}
