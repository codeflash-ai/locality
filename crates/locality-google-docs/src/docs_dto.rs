use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleDocument {
    #[serde(default)]
    pub document_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub revision_id: Option<String>,
    #[serde(default)]
    pub body: DocumentBody,
    #[serde(default)]
    pub lists: BTreeMap<String, List>,
    #[serde(default)]
    pub inline_objects: BTreeMap<String, InlineObject>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentBody {
    #[serde(default)]
    pub content: Vec<StructuralElement>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralElement {
    #[serde(default)]
    pub start_index: Option<usize>,
    #[serde(default)]
    pub end_index: Option<usize>,
    #[serde(default)]
    pub paragraph: Option<Paragraph>,
    #[serde(default)]
    pub table: Option<Table>,
    #[serde(default)]
    pub section_break: Option<serde_json::Value>,
    #[serde(default)]
    pub table_of_contents: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paragraph {
    #[serde(default)]
    pub elements: Vec<ParagraphElement>,
    #[serde(default)]
    pub paragraph_style: Option<ParagraphStyle>,
    #[serde(default)]
    pub bullet: Option<Bullet>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParagraphStyle {
    #[serde(default)]
    pub named_style_type: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bullet {
    #[serde(default)]
    pub list_id: Option<String>,
    #[serde(default)]
    pub nesting_level: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParagraphElement {
    #[serde(default)]
    pub start_index: Option<usize>,
    #[serde(default)]
    pub end_index: Option<usize>,
    #[serde(default)]
    pub text_run: Option<TextRun>,
    #[serde(default)]
    pub inline_object_element: Option<InlineObjectElement>,
    #[serde(default)]
    pub page_break: Option<serde_json::Value>,
    #[serde(default)]
    pub footnote_reference: Option<serde_json::Value>,
    #[serde(default)]
    pub equation: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextRun {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub text_style: TextStyle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextStyle {
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub strikethrough: bool,
    #[serde(default)]
    pub link: Option<Link>,
    #[serde(default)]
    pub foreground_color: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineObjectElement {
    #[serde(default)]
    pub inline_object_id: Option<String>,
    #[serde(default)]
    pub text_style: TextStyle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineObject {
    #[serde(default)]
    pub object_id: Option<String>,
    #[serde(default)]
    pub inline_object_properties: InlineObjectProperties,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineObjectProperties {
    #[serde(default)]
    pub embedded_object: Option<EmbeddedObject>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedObject {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub image_properties: Option<ImageProperties>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageProperties {
    #[serde(default)]
    pub content_uri: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Table {
    #[serde(default)]
    pub rows: Option<usize>,
    #[serde(default)]
    pub columns: Option<usize>,
    #[serde(default)]
    pub table_rows: Vec<TableRow>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableRow {
    #[serde(default)]
    pub table_cells: Vec<TableCell>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableCell {
    #[serde(default)]
    pub content: Vec<StructuralElement>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct List {
    #[serde(default)]
    pub list_properties: ListProperties,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListProperties {
    #[serde(default)]
    pub nesting_levels: Vec<NestingLevel>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestingLevel {
    #[serde(default)]
    pub glyph_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchUpdateDocumentRequest {
    pub requests: Vec<DocsRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_control: Option<WriteControl>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DocsRequest {
    DeleteContentRange {
        #[serde(rename = "deleteContentRange")]
        delete_content_range: DeleteContentRangeRequest,
    },
    InsertText {
        #[serde(rename = "insertText")]
        insert_text: InsertTextRequest,
    },
    UpdateTextStyle {
        #[serde(rename = "updateTextStyle")]
        update_text_style: UpdateTextStyleRequest,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteContentRangeRequest {
    pub range: Range,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsertTextRequest {
    pub location: Location,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTextStyleRequest {
    pub range: Range,
    pub text_style: TextStylePatch,
    pub fields: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextStylePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strikethrough: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_color: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Range {
    pub start_index: usize,
    pub end_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteControl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_revision_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        BatchUpdateDocumentRequest, DeleteContentRangeRequest, DocsRequest, GoogleDocument,
        InsertTextRequest, Range, TextStylePatch, UpdateTextStyleRequest, WriteControl,
    };

    #[test]
    fn document_decodes_paragraph_styles_links_lists_and_tables() {
        let payload = serde_json::json!({
            "documentId": "doc-1",
            "title": "Launch Brief",
            "revisionId": "rev-1",
            "body": {
                "content": [
                    {
                        "startIndex": 1,
                        "endIndex": 14,
                        "paragraph": {
                            "paragraphStyle": { "namedStyleType": "HEADING_1" },
                            "elements": [
                                {
                                    "startIndex": 1,
                                    "endIndex": 14,
                                    "textRun": {
                                        "content": "Launch Brief\n",
                                        "textStyle": { "bold": true }
                                    }
                                }
                            ]
                        }
                    },
                    {
                        "startIndex": 14,
                        "endIndex": 20,
                        "paragraph": {
                            "bullet": { "listId": "list-1", "nestingLevel": 0 },
                            "elements": [
                                {
                                    "startIndex": 14,
                                    "endIndex": 20,
                                    "textRun": {
                                        "content": "Item\n",
                                        "textStyle": {
                                            "link": { "url": "https://example.test" },
                                            "italic": true
                                        }
                                    }
                                }
                            ]
                        }
                    },
                    {
                        "startIndex": 20,
                        "endIndex": 40,
                        "table": {
                            "rows": 1,
                            "columns": 2,
                            "tableRows": [
                                {
                                    "tableCells": [
                                        { "content": [{ "paragraph": { "elements": [{ "textRun": { "content": "A\n" } }] } }] },
                                        { "content": [{ "paragraph": { "elements": [{ "textRun": { "content": "B\n" } }] } }] }
                                    ]
                                }
                            ]
                        }
                    }
                ]
            },
            "inlineObjects": {
                "obj-1": {
                    "objectId": "obj-1",
                    "inlineObjectProperties": {
                        "embeddedObject": {
                            "title": "Logo",
                            "description": "A circle with logo written in the center",
                            "imageProperties": {
                                "contentUri": "https://example.test/circle.png"
                            }
                        }
                    }
                }
            },
            "lists": {
                "list-1": {
                    "listProperties": {
                        "nestingLevels": [{ "glyphType": "BULLET" }]
                    }
                }
            }
        });

        let doc: GoogleDocument = serde_json::from_value(payload).expect("decode document");

        assert_eq!(doc.document_id, "doc-1");
        assert_eq!(doc.revision_id.as_deref(), Some("rev-1"));
        let heading = doc.body.content[0].paragraph.as_ref().expect("heading");
        assert_eq!(
            heading
                .paragraph_style
                .as_ref()
                .unwrap()
                .named_style_type
                .as_deref(),
            Some("HEADING_1")
        );
        assert!(
            heading.elements[0]
                .text_run
                .as_ref()
                .unwrap()
                .text_style
                .bold
        );
        assert_eq!(doc.body.content[2].table.as_ref().unwrap().columns, Some(2));
        assert_eq!(
            doc.inline_objects["obj-1"]
                .inline_object_properties
                .embedded_object
                .as_ref()
                .and_then(|object| object.image_properties.as_ref())
                .and_then(|properties| properties.content_uri.as_deref()),
            Some("https://example.test/circle.png")
        );
    }

    #[test]
    fn batch_update_serializes_required_revision_write_control() {
        let request = BatchUpdateDocumentRequest {
            requests: vec![
                DocsRequest::DeleteContentRange {
                    delete_content_range: DeleteContentRangeRequest {
                        range: Range {
                            start_index: 1,
                            end_index: 9,
                        },
                    },
                },
                DocsRequest::InsertText {
                    insert_text: InsertTextRequest {
                        location: super::Location { index: 1 },
                        text: "Updated\n".to_string(),
                    },
                },
                DocsRequest::UpdateTextStyle {
                    update_text_style: UpdateTextStyleRequest {
                        range: Range {
                            start_index: 1,
                            end_index: 8,
                        },
                        text_style: TextStylePatch {
                            bold: Some(true),
                            ..TextStylePatch::default()
                        },
                        fields: "bold".to_string(),
                    },
                },
            ],
            write_control: Some(WriteControl {
                required_revision_id: Some("rev-1".to_string()),
            }),
        };

        let json = serde_json::to_value(&request).expect("serialize batchUpdate");

        assert_eq!(json["writeControl"]["requiredRevisionId"], "rev-1");
        assert_eq!(
            json["requests"][0]["deleteContentRange"]["range"]["startIndex"],
            1
        );
        assert_eq!(json["requests"][1]["insertText"]["text"], "Updated\n");
        assert_eq!(json["requests"][2]["updateTextStyle"]["fields"], "bold");
        assert_eq!(
            json["requests"][2]["updateTextStyle"]["textStyle"]["bold"],
            true
        );
    }
}
