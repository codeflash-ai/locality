//! Local template pack support.
//!
//! Template packs are intentionally file-native. A pack is a directory with a
//! small YAML manifest and Markdown scaffolding that can be copied into a local
//! workspace without contacting any connector or remote registry.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MANIFEST_FILE: &str = ".agentfs-pack.yaml";
const LEGACY_MANIFEST_FILE: &str = "afs-pack.yaml";

const FOUNDER_PROOF_OF_WORK_MANIFEST: &str =
    include_str!("../../../templates/packs/founder-proof-of-work/.agentfs-pack.yaml");
const FOUNDER_PROOF_OF_WORK_FILES: &[EmbeddedTemplateFile] = &[
    EmbeddedTemplateFile {
        path: ".agentfs-pack.yaml",
        body: include_str!("../../../templates/packs/founder-proof-of-work/.agentfs-pack.yaml"),
    },
    EmbeddedTemplateFile {
        path: "README.md",
        body: include_str!("../../../templates/packs/founder-proof-of-work/README.md"),
    },
    EmbeddedTemplateFile {
        path: "index.md",
        body: include_str!("../../../templates/packs/founder-proof-of-work/index.md"),
    },
    EmbeddedTemplateFile {
        path: "log.md",
        body: include_str!("../../../templates/packs/founder-proof-of-work/log.md"),
    },
    EmbeddedTemplateFile {
        path: "templates/weekly-update.md",
        body: include_str!(
            "../../../templates/packs/founder-proof-of-work/templates/weekly-update.md"
        ),
    },
    EmbeddedTemplateFile {
        path: "templates/investor-update.md",
        body: include_str!(
            "../../../templates/packs/founder-proof-of-work/templates/investor-update.md"
        ),
    },
    EmbeddedTemplateFile {
        path: "templates/yc-application.md",
        body: include_str!(
            "../../../templates/packs/founder-proof-of-work/templates/yc-application.md"
        ),
    },
    EmbeddedTemplateFile {
        path: "templates/proof-of-work-site.md",
        body: include_str!(
            "../../../templates/packs/founder-proof-of-work/templates/proof-of-work-site.md"
        ),
    },
    EmbeddedTemplateFile {
        path: "workflows/summarize-week.md",
        body: include_str!(
            "../../../templates/packs/founder-proof-of-work/workflows/summarize-week.md"
        ),
    },
    EmbeddedTemplateFile {
        path: "workflows/create-deck.md",
        body: include_str!(
            "../../../templates/packs/founder-proof-of-work/workflows/create-deck.md"
        ),
    },
    EmbeddedTemplateFile {
        path: "policies/publish-rules.md",
        body: include_str!(
            "../../../templates/packs/founder-proof-of-work/policies/publish-rules.md"
        ),
    },
    EmbeddedTemplateFile {
        path: "outputs/README.md",
        body: include_str!("../../../templates/packs/founder-proof-of-work/outputs/README.md"),
    },
];

const FOCUSED_INBOX_MANIFEST: &str =
    include_str!("../../../templates/packs/focused-inbox/.agentfs-pack.yaml");
const FOCUSED_INBOX_FILES: &[EmbeddedTemplateFile] = &[
    EmbeddedTemplateFile {
        path: ".agentfs-pack.yaml",
        body: include_str!("../../../templates/packs/focused-inbox/.agentfs-pack.yaml"),
    },
    EmbeddedTemplateFile {
        path: "README.md",
        body: include_str!("../../../templates/packs/focused-inbox/README.md"),
    },
    EmbeddedTemplateFile {
        path: "today.md",
        body: include_str!("../../../templates/packs/focused-inbox/today.md"),
    },
    EmbeddedTemplateFile {
        path: "logs/decisions.md",
        body: include_str!("../../../templates/packs/focused-inbox/logs/decisions.md"),
    },
    EmbeddedTemplateFile {
        path: "templates/needs-reply.md",
        body: include_str!("../../../templates/packs/focused-inbox/templates/needs-reply.md"),
    },
    EmbeddedTemplateFile {
        path: "templates/source-filter.md",
        body: include_str!("../../../templates/packs/focused-inbox/templates/source-filter.md"),
    },
    EmbeddedTemplateFile {
        path: "workflows/triage.md",
        body: include_str!("../../../templates/packs/focused-inbox/workflows/triage.md"),
    },
    EmbeddedTemplateFile {
        path: "policies/focus-rules.md",
        body: include_str!("../../../templates/packs/focused-inbox/policies/focus-rules.md"),
    },
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplatePackManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub requires: TemplatePackRequirements,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub safety: TemplatePackSafety,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplatePackRequirements {
    #[serde(default)]
    pub connectors: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplatePackSafety {
    #[serde(default)]
    pub default_visibility: Option<String>,
    #[serde(default)]
    pub requires_review: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TemplatePackSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub source: String,
    pub requires: TemplatePackRequirements,
    pub outputs: Vec<String>,
    pub safety: TemplatePackSafety,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TemplateListReport {
    pub ok: bool,
    pub command: &'static str,
    pub packs: Vec<TemplatePackSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TemplateValidateReport {
    pub ok: bool,
    pub command: &'static str,
    pub path: String,
    pub pack: TemplatePackSummary,
    pub issues: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateNewOptions {
    pub pack: String,
    pub path: PathBuf,
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TemplateNewReport {
    pub ok: bool,
    pub command: &'static str,
    pub pack: TemplatePackSummary,
    pub path: String,
    pub files_written: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemplatePackError {
    PackNotFound(String),
    ManifestMissing(PathBuf),
    ManifestInvalid { path: PathBuf, message: String },
    InvalidPackId(String),
    InvalidRelativePath(String),
    TargetNotDirectory(PathBuf),
    TargetNotEmpty(PathBuf),
    FileExists(PathBuf),
    SymlinkUnsupported(PathBuf),
    Io(String),
}

impl TemplatePackError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::PackNotFound(_) => "pack_not_found",
            Self::ManifestMissing(_) => "manifest_missing",
            Self::ManifestInvalid { .. } => "manifest_invalid",
            Self::InvalidPackId(_) => "invalid_pack_id",
            Self::InvalidRelativePath(_) => "invalid_relative_path",
            Self::TargetNotDirectory(_) => "target_not_directory",
            Self::TargetNotEmpty(_) => "target_not_empty",
            Self::FileExists(_) => "file_exists",
            Self::SymlinkUnsupported(_) => "symlink_unsupported",
            Self::Io(_) => "io_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::PackNotFound(pack) => {
                format!("template pack `{pack}` was not found")
            }
            Self::ManifestMissing(path) => {
                format!(
                    "template pack manifest was not found under `{}`",
                    path.display()
                )
            }
            Self::ManifestInvalid { path, message } => {
                format!(
                    "template pack manifest `{}` is invalid: {message}",
                    path.display()
                )
            }
            Self::InvalidPackId(id) => {
                format!(
                    "template pack id `{id}` must use letters, numbers, dots, dashes, or underscores"
                )
            }
            Self::InvalidRelativePath(path) => {
                format!("template pack file path `{path}` is not a safe relative path")
            }
            Self::TargetNotDirectory(path) => {
                format!("target `{}` exists but is not a directory", path.display())
            }
            Self::TargetNotEmpty(path) => {
                format!(
                    "target `{}` is not empty; pass --force to overwrite matching files",
                    path.display()
                )
            }
            Self::FileExists(path) => {
                format!("target file `{}` already exists", path.display())
            }
            Self::SymlinkUnsupported(path) => {
                format!(
                    "template pack file `{}` is a symlink; symlinks are not supported",
                    path.display()
                )
            }
            Self::Io(message) => message.clone(),
        }
    }
}

pub fn run_template_list() -> Result<TemplateListReport, TemplatePackError> {
    Ok(TemplateListReport {
        ok: true,
        command: "templates_list",
        packs: first_party_pack_summaries()?,
    })
}

pub fn run_template_validate(path: PathBuf) -> Result<TemplateValidateReport, TemplatePackError> {
    let loaded = load_external_pack(&path)?;
    Ok(TemplateValidateReport {
        ok: true,
        command: "templates_validate",
        path: loaded.root.display().to_string(),
        pack: loaded.summary,
        issues: Vec::new(),
    })
}

pub fn run_template_new(
    options: TemplateNewOptions,
) -> Result<TemplateNewReport, TemplatePackError> {
    if options.path.exists() {
        if !options.path.is_dir() {
            return Err(TemplatePackError::TargetNotDirectory(options.path));
        }
        if !options.force && directory_has_entries(&options.path)? {
            return Err(TemplatePackError::TargetNotEmpty(options.path));
        }
    } else {
        fs::create_dir_all(&options.path)?;
    }

    let loaded = load_template_pack(&options.pack)?;
    let files_written = match &loaded.source {
        TemplatePackSource::Embedded(files) => {
            write_embedded_files(files, &options.path, options.force)?
        }
        TemplatePackSource::Local(root) => write_local_files(root, &options.path, options.force)?,
    };

    Ok(TemplateNewReport {
        ok: true,
        command: "templates_new",
        pack: loaded.summary,
        path: options.path.display().to_string(),
        files_written,
    })
}

fn first_party_pack_summaries() -> Result<Vec<TemplatePackSummary>, TemplatePackError> {
    first_party_packs()
        .iter()
        .map(|pack| {
            let manifest = parse_manifest(Path::new(pack.id), pack.manifest)?;
            validate_manifest(&manifest)?;
            Ok(summary_from_manifest(manifest, "first_party"))
        })
        .collect()
}

fn load_template_pack(pack: &str) -> Result<LoadedTemplatePack, TemplatePackError> {
    if let Some(embedded) = first_party_packs()
        .iter()
        .find(|candidate| candidate.id == pack)
    {
        let manifest = parse_manifest(Path::new(embedded.id), embedded.manifest)?;
        validate_manifest(&manifest)?;
        return Ok(LoadedTemplatePack {
            root: PathBuf::from(embedded.id),
            summary: summary_from_manifest(manifest, "first_party"),
            source: TemplatePackSource::Embedded(embedded.files),
        });
    }

    let path = PathBuf::from(pack);
    if path.exists() {
        return load_external_pack(&path);
    }

    Err(TemplatePackError::PackNotFound(pack.to_string()))
}

fn load_external_pack(path: &Path) -> Result<LoadedTemplatePack, TemplatePackError> {
    let root = if path.is_file() {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        path.to_path_buf()
    };
    let manifest_path = if path.is_file() {
        path.to_path_buf()
    } else {
        manifest_path_for_dir(&root)?
    };
    let body = fs::read_to_string(&manifest_path)?;
    let manifest = parse_manifest(&manifest_path, &body)?;
    validate_manifest(&manifest)?;

    Ok(LoadedTemplatePack {
        root: root.clone(),
        summary: summary_from_manifest(manifest, "local"),
        source: TemplatePackSource::Local(root),
    })
}

fn manifest_path_for_dir(root: &Path) -> Result<PathBuf, TemplatePackError> {
    for filename in [MANIFEST_FILE, LEGACY_MANIFEST_FILE] {
        let candidate = root.join(filename);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(TemplatePackError::ManifestMissing(root.to_path_buf()))
}

fn parse_manifest(path: &Path, body: &str) -> Result<TemplatePackManifest, TemplatePackError> {
    yaml_serde::from_str::<TemplatePackManifest>(body).map_err(|error| {
        TemplatePackError::ManifestInvalid {
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    })
}

fn validate_manifest(manifest: &TemplatePackManifest) -> Result<(), TemplatePackError> {
    if !valid_pack_id(&manifest.id) {
        return Err(TemplatePackError::InvalidPackId(manifest.id.clone()));
    }
    if manifest.name.trim().is_empty() {
        return Err(TemplatePackError::ManifestInvalid {
            path: PathBuf::from(&manifest.id),
            message: "name cannot be empty".to_string(),
        });
    }
    if manifest.version.trim().is_empty() {
        return Err(TemplatePackError::ManifestInvalid {
            path: PathBuf::from(&manifest.id),
            message: "version cannot be empty".to_string(),
        });
    }

    Ok(())
}

fn valid_pack_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'
                || character == '.'
        })
}

fn summary_from_manifest(manifest: TemplatePackManifest, source: &str) -> TemplatePackSummary {
    TemplatePackSummary {
        id: manifest.id,
        name: manifest.name,
        version: manifest.version,
        description: manifest.description,
        source: source.to_string(),
        requires: manifest.requires,
        outputs: manifest.outputs,
        safety: manifest.safety,
    }
}

fn write_embedded_files(
    files: &'static [EmbeddedTemplateFile],
    target: &Path,
    force: bool,
) -> Result<Vec<String>, TemplatePackError> {
    let mut written = Vec::new();
    for file in files {
        let relative_path = checked_relative_path(file.path)?;
        write_file(target, &relative_path, file.body.as_bytes(), force)?;
        written.push(relative_path_to_report(&relative_path));
    }
    Ok(written)
}

fn write_local_files(
    root: &Path,
    target: &Path,
    force: bool,
) -> Result<Vec<String>, TemplatePackError> {
    let mut files = list_local_pack_files(root)?;
    files.sort();
    let mut written = Vec::new();

    for source in files {
        let relative_path = source
            .strip_prefix(root)
            .map_err(|_| TemplatePackError::InvalidRelativePath(source.display().to_string()))?;
        let relative_path = checked_relative_path(&relative_path.to_string_lossy())?;
        let bytes = fs::read(&source)?;
        write_file(target, &relative_path, &bytes, force)?;
        written.push(relative_path_to_report(&relative_path));
    }

    Ok(written)
}

fn list_local_pack_files(root: &Path) -> Result<Vec<PathBuf>, TemplatePackError> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(TemplatePackError::SymlinkUnsupported(path));
            }
            if file_type.is_dir() {
                if entry.file_name() == ".git" {
                    continue;
                }
                pending.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }

    Ok(files)
}

fn checked_relative_path(path: &str) -> Result<PathBuf, TemplatePackError> {
    let relative_path = PathBuf::from(path);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(TemplatePackError::InvalidRelativePath(path.to_string()));
    }

    Ok(relative_path)
}

fn write_file(
    root: &Path,
    relative_path: &Path,
    bytes: &[u8],
    force: bool,
) -> Result<(), TemplatePackError> {
    let path = root.join(relative_path);
    if path.exists() && !force {
        return Err(TemplatePackError::FileExists(path));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn directory_has_entries(path: &Path) -> Result<bool, TemplatePackError> {
    Ok(fs::read_dir(path)?.next().is_some())
}

fn relative_path_to_report(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn first_party_packs() -> &'static [EmbeddedTemplatePack] {
    &[
        EmbeddedTemplatePack {
            id: "founder-proof-of-work",
            manifest: FOUNDER_PROOF_OF_WORK_MANIFEST,
            files: FOUNDER_PROOF_OF_WORK_FILES,
        },
        EmbeddedTemplatePack {
            id: "focused-inbox",
            manifest: FOCUSED_INBOX_MANIFEST,
            files: FOCUSED_INBOX_FILES,
        },
    ]
}

#[derive(Clone, Debug)]
struct EmbeddedTemplatePack {
    id: &'static str,
    manifest: &'static str,
    files: &'static [EmbeddedTemplateFile],
}

#[derive(Clone, Debug)]
struct EmbeddedTemplateFile {
    path: &'static str,
    body: &'static str,
}

#[derive(Clone, Debug)]
struct LoadedTemplatePack {
    #[allow(dead_code)]
    root: PathBuf,
    summary: TemplatePackSummary,
    source: TemplatePackSource,
}

#[derive(Clone, Debug)]
enum TemplatePackSource {
    Embedded(&'static [EmbeddedTemplateFile]),
    Local(PathBuf),
}

impl From<std::io::Error> for TemplatePackError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}
