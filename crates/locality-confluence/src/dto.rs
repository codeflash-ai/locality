use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredConfluenceCredential {
    pub site_url: String,
    pub email: String,
    pub api_token: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfluenceCollection<T> {
    #[serde(default)]
    pub results: Vec<T>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfluenceSpace {
    pub id: String,
    #[serde(default)]
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub homepage_id: Option<String>,
    #[serde(default, rename = "_links")]
    pub links: ConfluenceLinks,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfluencePage {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub space_id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub author_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub version: Option<ConfluenceVersion>,
    #[serde(default)]
    pub body: Option<ConfluencePageBody>,
    #[serde(default, rename = "_links")]
    pub links: ConfluenceLinks,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfluencePageBody {
    #[serde(default)]
    pub storage: Option<ConfluenceBodyRepresentation>,
    #[serde(default, rename = "atlas_doc_format")]
    pub atlas_doc_format: Option<ConfluenceBodyRepresentation>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfluenceBodyRepresentation {
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub representation: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfluenceVersion {
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfluenceLinks {
    #[serde(default)]
    pub webui: Option<String>,
}
