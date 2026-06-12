//! Shared identity, tree, hydration, and canonical-document types.
//!
//! The internal model keys entities by canonical remote IDs rather than paths.
//! Paths are a filesystem projection and can change when titles change. Equality
//! for sync decisions intentionally ignores operational state such as hydration
//! and compares only the projected entity fingerprint.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MountId(pub String);

impl MountId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RemoteId(pub String);

impl RemoteId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Page,
    Database,
    Directory,
    Asset,
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HydrationState {
    Virtual,
    Stub,
    Hydrated,
    Dirty,
    Conflicted,
}

impl HydrationState {
    pub fn can_transition_to(&self, next: &Self) -> bool {
        use HydrationState::*;

        matches!(
            (self, next),
            (Virtual, Stub)
                | (Stub, Hydrated)
                | (Hydrated, Dirty)
                | (Hydrated, Conflicted)
                | (Dirty, Hydrated)
                | (Dirty, Conflicted)
                | (Conflicted, Dirty)
                | (Conflicted, Hydrated)
        ) || self == next
    }

    pub fn transition_to(&self, next: Self) -> Result<Self, HydrationTransitionError> {
        if self.can_transition_to(&next) {
            Ok(next)
        } else {
            Err(HydrationTransitionError {
                from: self.clone(),
                to: next,
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydrationTransitionError {
    pub from: HydrationState,
    pub to: HydrationState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeKind {
    Remote,
    Local,
    Synced,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub mount_id: MountId,
    pub remote_id: RemoteId,
    pub kind: EntityKind,
    pub title: String,
    pub path: PathBuf,
    pub hydration: HydrationState,
    pub content_hash: Option<String>,
    pub remote_edited_at: Option<String>,
    /// Connector-rendered frontmatter to use when writing an unhydrated stub.
    ///
    /// The durable store tracks only identity and sync state; this transient
    /// projection field lets connectors preserve rich metadata, such as
    /// database row properties, during mount-root enumeration.
    #[serde(default)]
    pub stub_frontmatter: Option<String>,
}

impl TreeEntry {
    pub fn fingerprint(&self) -> EntryFingerprint {
        EntryFingerprint {
            kind: self.kind.clone(),
            title: self.title.clone(),
            path: self.path.clone(),
            content_hash: self.content_hash.clone(),
            remote_edited_at: self.remote_edited_at.clone(),
        }
    }

    pub fn differs_from(&self, synced: &Self) -> bool {
        self.fingerprint() != synced.fingerprint()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryFingerprint {
    pub kind: EntityKind,
    pub title: String,
    pub path: PathBuf,
    pub content_hash: Option<String>,
    pub remote_edited_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalDocument {
    pub frontmatter: String,
    pub body: String,
    pub blocks: Vec<CanonicalBlock>,
}

impl CanonicalDocument {
    pub const STUB_MARKER: &'static str =
        "<!-- afs:stub — read triggers hydration, or run: afs pull <path> -->";

    pub fn new(frontmatter: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            frontmatter: frontmatter.into(),
            body: body.into(),
            blocks: Vec::new(),
        }
    }

    pub fn with_blocks(mut self, blocks: Vec<CanonicalBlock>) -> Self {
        self.blocks = blocks;
        self
    }

    pub fn is_stub(&self) -> bool {
        self.body.trim() == Self::STUB_MARKER
    }

    pub fn empty_stub() -> Self {
        Self {
            frontmatter: String::new(),
            body: format!("{}\n", Self::STUB_MARKER),
            blocks: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalBlock {
    pub remote_id: Option<RemoteId>,
    pub kind: BlockKind,
    pub source_span: Option<SourceSpan>,
    pub content_hash: Option<String>,
}

impl CanonicalBlock {
    pub fn native(remote_id: Option<RemoteId>, content_hash: Option<String>) -> Self {
        Self {
            remote_id,
            kind: BlockKind::NativeMarkdown,
            source_span: None,
            content_hash,
        }
    }

    pub fn directive(
        remote_id: RemoteId,
        directive_type: impl Into<String>,
        raw: impl Into<String>,
    ) -> Self {
        Self {
            remote_id: Some(remote_id),
            kind: BlockKind::Directive {
                directive_type: directive_type.into(),
                raw: raw.into(),
            },
            source_span: None,
            content_hash: None,
        }
    }

    pub fn parsed_directive(
        remote_id: Option<RemoteId>,
        directive_type: Option<String>,
        raw: impl Into<String>,
        line: usize,
    ) -> Self {
        Self {
            remote_id,
            kind: BlockKind::Directive {
                directive_type: directive_type.unwrap_or_default(),
                raw: raw.into(),
            },
            source_span: Some(SourceSpan {
                start_line: line,
                end_line: line,
            }),
            content_hash: None,
        }
    }

    pub fn directive_parts(&self) -> Option<(&RemoteId, &str, &str)> {
        match (&self.remote_id, &self.kind) {
            (
                Some(remote_id),
                BlockKind::Directive {
                    directive_type,
                    raw,
                },
            ) => Some((remote_id, directive_type.as_str(), raw.as_str())),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockKind {
    NativeMarkdown,
    Directive { directive_type: String, raw: String },
    Structural,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start_line: usize,
    pub end_line: usize,
}
