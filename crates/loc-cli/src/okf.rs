//! Open Knowledge Format export helpers.
//!
//! This is intentionally an offline projection over already-mounted Markdown.
//! It does not call connectors, mutate Locality state, or participate in push.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use locality_core::canonical::{CanonicalParseError, parse_canonical_markdown};
use locality_core::model::EntityKind;
use serde::Serialize;
use yaml_serde::{Mapping, Value};

const PAGE_DOCUMENT_FILENAME: &str = "page.md";
const ROOT_CONCEPT_STEM: &str = "root";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OkfExportOptions {
    pub source: PathBuf,
    pub output: PathBuf,
    pub connector: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OkfExportReport {
    pub ok: bool,
    pub command: &'static str,
    pub source: String,
    pub output: String,
    pub concepts: usize,
    pub indexes: usize,
    pub skipped: Vec<OkfSkippedFile>,
    pub files_written: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OkfSkippedFile {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExportedConcept {
    title: String,
    description: Option<String>,
    source_rel: PathBuf,
    output_rel: PathBuf,
    contents: String,
}

#[derive(Clone, Debug, Default)]
struct IndexDirectory {
    concepts: Vec<IndexConcept>,
    child_dirs: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct IndexConcept {
    title: String,
    description: Option<String>,
    href: String,
}

pub fn run_okf_export(options: OkfExportOptions) -> Result<OkfExportReport, OkfExportError> {
    let source = canonical_source(&options.source)?;
    let output = absolute_path(&options.output)?;
    if output.starts_with(&source) {
        return Err(OkfExportError::OutputInsideSource { source, output });
    }
    prepare_output_directory(&output)?;

    let markdown_files = collect_markdown_files(&source)?;
    let mut output_paths = BTreeSet::new();
    let mut concepts = Vec::new();
    let mut skipped = Vec::new();

    for source_file in markdown_files {
        let source_rel = source_file
            .strip_prefix(&source)
            .expect("collected source file should be below source")
            .to_path_buf();
        let source_rel_display = slash_path(&source_rel);
        let raw = match fs::read_to_string(&source_file) {
            Ok(raw) => raw,
            Err(error) => {
                skipped.push(OkfSkippedFile {
                    path: source_rel_display,
                    reason: format!("read_failed: {error}"),
                });
                continue;
            }
        };
        let parsed = match parse_canonical_markdown(&raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                skipped.push(OkfSkippedFile {
                    path: source_rel_display,
                    reason: parse_skip_reason(error),
                });
                continue;
            }
        };
        if parsed.is_stub() {
            skipped.push(OkfSkippedFile {
                path: source_rel_display,
                reason: "stub_not_exported".to_string(),
            });
            continue;
        }

        let output_rel = okf_output_path(&source, &source_rel);
        if !output_paths.insert(output_rel.clone()) {
            return Err(OkfExportError::OutputPathConflict {
                path: output_rel.clone(),
            });
        }

        let title = parsed
            .frontmatter
            .title
            .clone()
            .unwrap_or_else(|| title_from_source_path(&source, &source_rel));
        let description = string_property(&parsed.frontmatter.properties, "description");
        let connector =
            original_connector(&parsed.document.frontmatter).or_else(|| options.connector.clone());
        let contents = render_okf_concept(OkfConceptRender {
            title: &title,
            source_rel: &source_rel,
            parsed: &parsed,
            connector: connector.as_deref(),
        })?;

        concepts.push(ExportedConcept {
            title,
            description,
            source_rel,
            output_rel,
            contents,
        });
    }

    let mut index = BTreeMap::<PathBuf, IndexDirectory>::new();
    index.entry(PathBuf::new()).or_default();
    for concept in &concepts {
        record_index_entry(&mut index, concept);
    }

    let mut files_written = Vec::new();
    for concept in &concepts {
        let target = output.join(&concept.output_rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| OkfExportError::WriteFile {
                path: parent.to_path_buf(),
                message: error.to_string(),
            })?;
        }
        fs::write(&target, &concept.contents).map_err(|error| OkfExportError::WriteFile {
            path: target.clone(),
            message: error.to_string(),
        })?;
        files_written.push(slash_path(&concept.output_rel));
    }

    let index_count = index.len();
    for (directory, listing) in &index {
        let target = output.join(directory).join("index.md");
        fs::create_dir_all(target.parent().unwrap_or(&output)).map_err(|error| {
            OkfExportError::WriteFile {
                path: target.parent().unwrap_or(&output).to_path_buf(),
                message: error.to_string(),
            }
        })?;
        fs::write(&target, render_index(directory, listing)).map_err(|error| {
            OkfExportError::WriteFile {
                path: target.clone(),
                message: error.to_string(),
            }
        })?;
        files_written.push(slash_path(&directory.join("index.md")));
    }

    files_written.sort();
    Ok(OkfExportReport {
        ok: true,
        command: "okf_export",
        source: source.display().to_string(),
        output: output.display().to_string(),
        concepts: concepts.len(),
        indexes: index_count,
        skipped,
        files_written,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OkfExportError {
    CurrentDir { message: String },
    OutputInsideSource { source: PathBuf, output: PathBuf },
    OutputNotDirectory(PathBuf),
    OutputNotEmpty(PathBuf),
    OutputPathConflict { path: PathBuf },
    SourceMissing(PathBuf),
    SourceNotDirectory(PathBuf),
    WalkDirectory { path: PathBuf, message: String },
    WriteFile { path: PathBuf, message: String },
    YamlSerialize(String),
}

impl OkfExportError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CurrentDir { .. } => "current_dir_failed",
            Self::OutputInsideSource { .. } => "output_inside_source",
            Self::OutputNotDirectory(_) => "output_not_directory",
            Self::OutputNotEmpty(_) => "output_not_empty",
            Self::OutputPathConflict { .. } => "output_path_conflict",
            Self::SourceMissing(_) => "source_missing",
            Self::SourceNotDirectory(_) => "source_not_directory",
            Self::WalkDirectory { .. } => "walk_directory_failed",
            Self::WriteFile { .. } => "write_file_failed",
            Self::YamlSerialize(_) => "yaml_serialize_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CurrentDir { message } => {
                format!("failed to resolve current directory: {message}")
            }
            Self::OutputInsideSource { source, output } => format!(
                "OKF output `{}` must not be inside source `{}`",
                output.display(),
                source.display()
            ),
            Self::OutputNotDirectory(path) => {
                format!(
                    "OKF output `{}` exists but is not a directory",
                    path.display()
                )
            }
            Self::OutputNotEmpty(path) => {
                format!(
                    "OKF output `{}` already exists and is not empty",
                    path.display()
                )
            }
            Self::OutputPathConflict { path } => {
                format!("multiple source files would export to `{}`", path.display())
            }
            Self::SourceMissing(path) => {
                format!("OKF source `{}` does not exist", path.display())
            }
            Self::SourceNotDirectory(path) => {
                format!("OKF source `{}` is not a directory", path.display())
            }
            Self::WalkDirectory { path, message } => {
                format!("failed to read `{}`: {message}", path.display())
            }
            Self::WriteFile { path, message } => {
                format!("failed to write `{}`: {message}", path.display())
            }
            Self::YamlSerialize(message) => {
                format!("failed to render OKF frontmatter: {message}")
            }
        }
    }
}

struct OkfConceptRender<'a> {
    title: &'a str,
    source_rel: &'a Path,
    parsed: &'a locality_core::canonical::ParsedCanonicalDocument,
    connector: Option<&'a str>,
}

fn render_okf_concept(input: OkfConceptRender<'_>) -> Result<String, OkfExportError> {
    let mut frontmatter = Mapping::new();
    let properties = &input.parsed.frontmatter.properties;
    let okf_type = string_property(properties, "okf_type")
        .or_else(|| string_property(properties, "type"))
        .unwrap_or_else(|| default_okf_type(input.parsed, input.connector));

    insert_string(&mut frontmatter, "type", okf_type);
    insert_string(&mut frontmatter, "title", input.title.to_string());
    insert_optional_string(
        &mut frontmatter,
        "description",
        string_property(properties, "description"),
    );
    insert_optional_string(
        &mut frontmatter,
        "resource",
        string_property(properties, "resource"),
    );
    if let Some(tags) = properties.get("tags").filter(|value| is_sequence(value)) {
        frontmatter.insert(Value::String("tags".to_string()), tags.clone());
    }
    insert_optional_string(
        &mut frontmatter,
        "timestamp",
        string_property(properties, "timestamp").or_else(|| remote_timestamp(input.parsed)),
    );

    for (key, value) in properties {
        if matches!(
            key.as_str(),
            "okf_type" | "type" | "description" | "resource" | "tags" | "timestamp"
        ) {
            continue;
        }
        let key = if key == "locality" {
            "source_locality".to_string()
        } else {
            key.clone()
        };
        frontmatter.insert(Value::String(key), value.clone());
    }
    frontmatter.insert(
        Value::String("locality".to_string()),
        Value::Mapping(locality_extension(&input)),
    );

    let mut output = String::from("---\n");
    output.push_str(&frontmatter_yaml(&frontmatter)?);
    output.push_str("---\n");
    output.push_str(&input.parsed.document.body);
    Ok(output)
}

fn locality_extension(input: &OkfConceptRender<'_>) -> Mapping {
    let mut locality = Mapping::new();
    insert_string(&mut locality, "format", "locality-canonical".to_string());
    insert_string(&mut locality, "source_path", slash_path(input.source_rel));
    if let Some(remote_id) = input.parsed.remote_id() {
        insert_string(&mut locality, "remote_id", remote_id.as_str().to_string());
    }
    if let Some(loc) = &input.parsed.frontmatter.loc {
        if let Some(entity_type) = &loc.raw_entity_type {
            insert_string(&mut locality, "entity_type", entity_type.clone());
        }
        if let Some(parent) = &loc.parent {
            insert_string(&mut locality, "parent_id", parent.as_str().to_string());
        }
        if let Some(synced_at) = &loc.synced_at {
            insert_string(&mut locality, "synced_at", synced_at.clone());
        }
        if let Some(remote_edited_at) = &loc.remote_edited_at {
            insert_string(&mut locality, "remote_edited_at", remote_edited_at.clone());
        }
    }
    if let Some(connector) = input.connector {
        insert_string(&mut locality, "connector", connector.to_string());
    }
    locality
}

fn default_okf_type(
    parsed: &locality_core::canonical::ParsedCanonicalDocument,
    connector: Option<&str>,
) -> String {
    let source = connector
        .map(connector_display_name)
        .unwrap_or_else(|| "Locality".to_string());
    let kind = parsed
        .frontmatter
        .loc
        .as_ref()
        .and_then(|metadata| metadata.entity_type.as_ref())
        .map(entity_kind_label)
        .unwrap_or("Concept");
    format!("{source} {kind}")
}

fn entity_kind_label(kind: &EntityKind) -> &'static str {
    match kind {
        EntityKind::Page => "Page",
        EntityKind::Database => "Database",
        EntityKind::Directory => "Directory",
        EntityKind::Asset => "Asset",
        EntityKind::Unknown(_) => "Concept",
    }
}

fn connector_display_name(connector: &str) -> String {
    connector
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn remote_timestamp(parsed: &locality_core::canonical::ParsedCanonicalDocument) -> Option<String> {
    let value = parsed
        .frontmatter
        .loc
        .as_ref()
        .and_then(|metadata| metadata.remote_edited_at.clone())?;
    if value.contains('T') && value.contains('-') {
        Some(value)
    } else {
        None
    }
}

fn frontmatter_yaml(frontmatter: &Mapping) -> Result<String, OkfExportError> {
    let rendered = yaml_serde::to_string(frontmatter)
        .map_err(|error| OkfExportError::YamlSerialize(error.to_string()))?;
    Ok(strip_yaml_document_marker(&rendered).to_string())
}

fn strip_yaml_document_marker(rendered: &str) -> &str {
    rendered
        .strip_prefix("---\n")
        .unwrap_or(rendered)
        .trim_end_matches("...\n")
}

fn collect_markdown_files(source: &Path) -> Result<Vec<PathBuf>, OkfExportError> {
    let mut files = Vec::new();
    collect_markdown_files_inner(source, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_markdown_files_inner(
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), OkfExportError> {
    for entry in fs::read_dir(directory).map_err(|error| OkfExportError::WalkDirectory {
        path: directory.to_path_buf(),
        message: error.to_string(),
    })? {
        let entry = entry.map_err(|error| OkfExportError::WalkDirectory {
            path: directory.to_path_buf(),
            message: error.to_string(),
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| OkfExportError::WalkDirectory {
                path: path.clone(),
                message: error.to_string(),
            })?;
        if file_type.is_dir() {
            if should_skip_directory(&path) {
                continue;
            }
            collect_markdown_files_inner(&path, files)?;
        } else if file_type.is_file() && should_export_markdown_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn should_skip_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, ".git" | ".loc" | "node_modules"))
}

fn should_export_markdown_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if matches!(name, "AGENTS.md" | "CLAUDE.md") {
        return false;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

fn okf_output_path(source: &Path, source_rel: &Path) -> PathBuf {
    if source_rel
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == PAGE_DOCUMENT_FILENAME)
    {
        let parent = source_rel.parent().unwrap_or_else(|| Path::new(""));
        if parent.as_os_str().is_empty() {
            let stem = source
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(ROOT_CONCEPT_STEM);
            return PathBuf::from(format!("{stem}.md"));
        }
        return parent.with_extension("md");
    }

    let file_name = source_rel
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(file_name, "index.md" | "log.md") {
        return source_rel.with_file_name(format!("_{}", file_name));
    }
    source_rel.to_path_buf()
}

fn title_from_source_path(source: &Path, source_rel: &Path) -> String {
    if source_rel
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == PAGE_DOCUMENT_FILENAME)
    {
        return source_rel
            .parent()
            .and_then(|parent| parent.file_name())
            .or_else(|| source.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("Untitled")
            .to_string();
    }
    source_rel
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Untitled")
        .to_string()
}

fn record_index_entry(index: &mut BTreeMap<PathBuf, IndexDirectory>, concept: &ExportedConcept) {
    let parent = concept
        .output_rel
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_path_buf();
    index
        .entry(parent.clone())
        .or_default()
        .concepts
        .push(IndexConcept {
            title: concept.title.clone(),
            description: concept.description.clone(),
            href: concept
                .output_rel
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
        });

    let mut current = PathBuf::new();
    index.entry(current.clone()).or_default();
    for component in parent.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let child = name.to_string_lossy().to_string();
        index
            .entry(current.clone())
            .or_default()
            .child_dirs
            .insert(child.clone());
        current.push(child);
        index.entry(current.clone()).or_default();
    }
}

fn render_index(directory: &Path, listing: &IndexDirectory) -> String {
    let heading = if directory.as_os_str().is_empty() {
        "OKF Bundle".to_string()
    } else {
        directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Index")
            .to_string()
    };
    let mut output = format!("# {heading}\n\n");
    if listing.child_dirs.is_empty() && listing.concepts.is_empty() {
        output.push_str("* No exported concepts.\n");
        return output;
    }
    for child in &listing.child_dirs {
        output.push_str(&format!("* [{}]({}/) - Directory\n", child, child));
    }
    for concept in &listing.concepts {
        let description = concept.description.as_deref().unwrap_or("Concept");
        output.push_str(&format!(
            "* [{}]({}) - {}\n",
            concept.title, concept.href, description
        ));
    }
    output
}

fn canonical_source(source: &Path) -> Result<PathBuf, OkfExportError> {
    if !source.exists() {
        return Err(OkfExportError::SourceMissing(source.to_path_buf()));
    }
    if !source.is_dir() {
        return Err(OkfExportError::SourceNotDirectory(source.to_path_buf()));
    }
    fs::canonicalize(source).map_err(|error| OkfExportError::WalkDirectory {
        path: source.to_path_buf(),
        message: error.to_string(),
    })
}

fn prepare_output_directory(output: &Path) -> Result<(), OkfExportError> {
    if output.exists() {
        if !output.is_dir() {
            return Err(OkfExportError::OutputNotDirectory(output.to_path_buf()));
        }
        if fs::read_dir(output)
            .map_err(|error| OkfExportError::WalkDirectory {
                path: output.to_path_buf(),
                message: error.to_string(),
            })?
            .next()
            .is_some()
        {
            return Err(OkfExportError::OutputNotEmpty(output.to_path_buf()));
        }
        return Ok(());
    }
    fs::create_dir_all(output).map_err(|error| OkfExportError::WriteFile {
        path: output.to_path_buf(),
        message: error.to_string(),
    })
}

fn absolute_path(path: &Path) -> Result<PathBuf, OkfExportError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| OkfExportError::CurrentDir {
                message: error.to_string(),
            })
    }
}

fn string_property(properties: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    match properties.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.clone()),
        _ => None,
    }
}

fn original_connector(frontmatter: &str) -> Option<String> {
    let value = yaml_serde::from_str::<Value>(frontmatter).ok()?;
    let Value::Mapping(root) = value else {
        return None;
    };
    let loc = root
        .get("loc")
        .or_else(|| root.get("afs"))
        .and_then(Value::as_mapping)?;
    match loc.get("connector") {
        Some(Value::String(connector)) if !connector.trim().is_empty() => Some(connector.clone()),
        _ => None,
    }
}

fn is_sequence(value: &Value) -> bool {
    matches!(value, Value::Sequence(_))
}

fn insert_string(mapping: &mut Mapping, key: &str, value: String) {
    mapping.insert(Value::String(key.to_string()), Value::String(value));
}

fn insert_optional_string(mapping: &mut Mapping, key: &str, value: Option<String>) {
    if let Some(value) = value {
        insert_string(mapping, key, value);
    }
}

fn parse_skip_reason(error: CanonicalParseError) -> String {
    format!("invalid_canonical_markdown: {}", error.message)
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            Component::CurDir => None,
            Component::ParentDir => Some("..".to_string()),
            Component::RootDir => Some(String::new()),
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().to_string()),
        })
        .collect::<Vec<_>>()
        .join("/")
}

impl From<io::Error> for OkfExportError {
    fn from(error: io::Error) -> Self {
        Self::WriteFile {
            path: PathBuf::new(),
            message: error.to_string(),
        }
    }
}
