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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateApplyOptions {
    pub pack: String,
    pub template: String,
    pub target_dir: PathBuf,
    pub title: Option<String>,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TemplateApplyReport {
    pub ok: bool,
    pub command: &'static str,
    pub pack: TemplatePackSummary,
    pub template: String,
    pub path: String,
    pub suggested_next: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemplatePackError {
    PackNotFound(String),
    TemplateNotFound { pack: String, template: String },
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
            Self::TemplateNotFound { .. } => "template_not_found",
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
            Self::TemplateNotFound { pack, template } => {
                format!("template `{template}` was not found in pack `{pack}`")
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

pub fn run_template_apply(
    options: TemplateApplyOptions,
) -> Result<TemplateApplyReport, TemplatePackError> {
    if options.target_dir.exists() && !options.target_dir.is_dir() {
        return Err(TemplatePackError::TargetNotDirectory(options.target_dir));
    }
    fs::create_dir_all(&options.target_dir)?;

    let loaded = load_template_pack(&options.pack)?;
    let template = read_template_file(&loaded, &options.template)?;
    let title = options
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let body = render_template_body(&template.body, title, &loaded.summary.id);
    let filename = title.map(markdown_filename_for_title).unwrap_or_else(|| {
        template
            .relative_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| "draft.md".to_string())
    });
    let output_relative_path = checked_relative_path(&filename)?;
    let output_path = options.target_dir.join(&output_relative_path);
    write_file(
        &options.target_dir,
        &output_relative_path,
        body.as_bytes(),
        options.force,
    )?;

    let path = output_path.display().to_string();
    Ok(TemplateApplyReport {
        ok: true,
        command: "templates_apply",
        pack: loaded.summary,
        template: relative_path_to_report(&template.relative_path),
        path: path.clone(),
        suggested_next: vec![
            format!("afs diff {}", shell_quote(&path)),
            format!("afs push {} -y", shell_quote(&path)),
        ],
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

fn read_template_file(
    loaded: &LoadedTemplatePack,
    template: &str,
) -> Result<LoadedTemplateFile, TemplatePackError> {
    let candidates = template_path_candidates(template)?;
    match &loaded.source {
        TemplatePackSource::Embedded(files) => files
            .iter()
            .find(|file| {
                candidates
                    .iter()
                    .any(|candidate| candidate == Path::new(file.path))
            })
            .map(|file| LoadedTemplateFile {
                relative_path: PathBuf::from(file.path),
                body: file.body.to_string(),
            })
            .ok_or_else(|| TemplatePackError::TemplateNotFound {
                pack: loaded.summary.id.clone(),
                template: template.to_string(),
            }),
        TemplatePackSource::Local(root) => {
            for candidate in candidates {
                let path = root.join(&candidate);
                if path.is_file() {
                    return Ok(LoadedTemplateFile {
                        relative_path: candidate,
                        body: fs::read_to_string(path)?,
                    });
                }
            }
            Err(TemplatePackError::TemplateNotFound {
                pack: loaded.summary.id.clone(),
                template: template.to_string(),
            })
        }
    }
}

fn template_path_candidates(template: &str) -> Result<Vec<PathBuf>, TemplatePackError> {
    let mut value = template.trim().trim_matches('/').to_string();
    if value.is_empty() {
        return Err(TemplatePackError::InvalidRelativePath(template.to_string()));
    }
    if !value.ends_with(".md") {
        value.push_str(".md");
    }

    let direct = checked_relative_path(&value)?;
    let prefixed = if direct.components().count() == 1 {
        checked_relative_path(&format!("templates/{value}"))?
    } else {
        direct.clone()
    };

    let mut candidates = vec![prefixed, direct];
    candidates.dedup();
    Ok(candidates)
}

fn render_template_body(body: &str, title: Option<&str>, pack_id: &str) -> String {
    let today = current_utc_date();
    let rendered = body
        .replace("{{pack_id}}", pack_id)
        .replace("{{date}}", &today);
    let Some(title) = title else {
        return rendered;
    };
    replace_frontmatter_title(&rendered, title).replace("{{title}}", title)
}

fn replace_frontmatter_title(body: &str, title: &str) -> String {
    let Some((frontmatter, rest)) = split_frontmatter_body(body) else {
        return body.to_string();
    };
    let title_line = format!("title: \"{}\"", yaml_double_quoted(title));
    let mut replaced = false;
    let mut next_frontmatter = Vec::new();
    for line in frontmatter.lines() {
        if line.trim_start().starts_with("title:") {
            next_frontmatter.push(title_line.clone());
            replaced = true;
        } else {
            next_frontmatter.push(line.to_string());
        }
    }
    if !replaced {
        next_frontmatter.insert(0, title_line);
    }

    format!("---\n{}\n{}", next_frontmatter.join("\n"), rest)
}

fn split_frontmatter_body(body: &str) -> Option<(&str, &str)> {
    let content = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))?;
    for marker in ["\n---\n", "\r\n---\r\n", "\n---\r\n", "\r\n---\n"] {
        if let Some(index) = content.find(marker) {
            return Some((&content[..index], &content[index + marker.len()..]));
        }
    }
    None
}

fn yaml_double_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn markdown_filename_for_title(title: &str) -> String {
    let sanitized = title
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            character if character.is_control() => '-',
            character => character,
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string();
    let stem = if sanitized.is_empty() {
        "draft".to_string()
    } else {
        sanitized
    };
    if stem.ends_with(".md") {
        stem
    } else {
        format!("{stem}.md")
    }
}

fn current_utc_date() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() / 86_400)
        .unwrap_or(0);
    civil_date_from_unix_days(days as i64)
}

fn civil_date_from_unix_days(days: i64) -> String {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02}")
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

fn shell_quote(value: &str) -> String {
    if value.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '/' | '.' | '_' | '-')
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
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
struct LoadedTemplateFile {
    relative_path: PathBuf,
    body: String,
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
