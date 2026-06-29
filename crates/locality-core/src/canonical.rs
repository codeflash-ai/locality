//! Canonical Markdown parsing and rendering.
//!
//! Locality stores connector-rendered entities as Markdown plus YAML
//! frontmatter. This module parses only the connector-neutral structure:
//! frontmatter envelope, Locality identity metadata, stub marker detection, and
//! directive lines. It deliberately does not parse Notion block semantics; that
//! belongs in connectors and the block diff engine.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use serde::Deserialize;
use yaml_serde::Value;

use crate::model::{CanonicalBlock, CanonicalDocument, EntityKind, RemoteId, SourceSpan};

pub type FrontmatterProperties = BTreeMap<String, Value>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCanonicalDocument {
    pub document: CanonicalDocument,
    pub frontmatter: Frontmatter,
    pub directives: Vec<Directive>,
    pub body_start_line: usize,
}

impl ParsedCanonicalDocument {
    pub fn is_stub(&self) -> bool {
        self.document.is_stub()
    }

    pub fn remote_id(&self) -> Option<&RemoteId> {
        self.frontmatter
            .loc
            .as_ref()
            .and_then(|metadata| metadata.id.as_ref())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frontmatter {
    pub loc: Option<LocalityMetadata>,
    pub title: Option<String>,
    pub properties: FrontmatterProperties,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalityMetadata {
    pub id: Option<RemoteId>,
    pub entity_type: Option<EntityKind>,
    pub raw_entity_type: Option<String>,
    pub parent: Option<RemoteId>,
    pub synced_at: Option<String>,
    pub remote_edited_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directive {
    pub remote_id: Option<RemoteId>,
    pub directive_type: Option<String>,
    pub title: Option<String>,
    pub attributes: BTreeMap<String, String>,
    pub raw: String,
    pub line: usize,
    pub malformed: bool,
}

impl Directive {
    pub fn source_span(&self) -> SourceSpan {
        SourceSpan {
            start_line: self.line,
            end_line: self.line,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalParseError {
    pub kind: CanonicalParseErrorKind,
    pub line: Option<usize>,
    pub message: String,
}

impl Display for CanonicalParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "{} at line {}", self.message, line),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for CanonicalParseError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalParseErrorKind {
    MissingFrontmatter,
    UnterminatedFrontmatter,
    InvalidFrontmatterYaml,
}

pub fn parse_canonical_markdown(
    input: &str,
) -> Result<ParsedCanonicalDocument, CanonicalParseError> {
    let split = split_frontmatter(input)?;
    let frontmatter = parse_frontmatter(split.frontmatter)?;
    let directives = extract_directives(split.body, split.body_start_line);
    let blocks = directives
        .iter()
        .map(|directive| {
            CanonicalBlock::parsed_directive(
                directive.remote_id.clone(),
                directive.directive_type.clone(),
                directive.raw.clone(),
                directive.line,
            )
        })
        .collect();
    let document = CanonicalDocument::new(split.frontmatter, split.body).with_blocks(blocks);

    Ok(ParsedCanonicalDocument {
        document,
        frontmatter,
        directives,
        body_start_line: split.body_start_line,
    })
}

pub fn render_canonical_markdown(document: &CanonicalDocument) -> String {
    let mut rendered = String::from("---\n");
    rendered.push_str(&document.frontmatter);
    if !document.frontmatter.is_empty() && !document.frontmatter.ends_with('\n') {
        rendered.push('\n');
    }
    rendered.push_str("---\n");
    rendered.push_str(&document.body);
    rendered
}

pub fn is_stub_body(body: &str) -> bool {
    body.trim() == CanonicalDocument::STUB_MARKER
}

pub fn extract_directives(body: &str, body_start_line: usize) -> Vec<Directive> {
    body.lines()
        .enumerate()
        .filter_map(|(offset, line)| parse_directive_line(line, body_start_line + offset))
        .collect()
}

pub fn parse_directive_line(line: &str, line_number: usize) -> Option<Directive> {
    let raw = line.to_string();
    let trimmed = line.trim();

    if !trimmed.starts_with("::loc") {
        return None;
    }

    let malformed = !trimmed.starts_with("::loc{") || !trimmed.ends_with('}');
    let attributes = if malformed {
        BTreeMap::new()
    } else {
        parse_directive_attributes(&trimmed["::loc{".len()..trimmed.len() - 1])
    };
    let remote_id = attributes.get("id").filter(|id| !id.is_empty()).cloned();
    let directive_type = attributes
        .get("type")
        .filter(|directive_type| !directive_type.is_empty())
        .cloned();
    let title = attributes
        .get("title")
        .filter(|title| !title.is_empty())
        .cloned();

    Some(Directive {
        remote_id: remote_id.map(RemoteId::new),
        directive_type,
        title,
        attributes,
        raw,
        line: line_number,
        malformed,
    })
}

fn parse_frontmatter(frontmatter: &str) -> Result<Frontmatter, CanonicalParseError> {
    let raw = if frontmatter.trim().is_empty() {
        RawFrontmatter::default()
    } else {
        yaml_serde::from_str::<RawFrontmatter>(frontmatter).map_err(|error| {
            CanonicalParseError {
                kind: CanonicalParseErrorKind::InvalidFrontmatterYaml,
                line: error.location().map(|location| location.line() + 1),
                message: format!("invalid YAML frontmatter: {error}"),
            }
        })?
    };

    Ok(raw.into_frontmatter())
}

fn split_frontmatter(input: &str) -> Result<FrontmatterSplit<'_>, CanonicalParseError> {
    let mut cursor = 0;
    let mut lines = input.split_inclusive('\n');
    let Some(first_line) = lines.next() else {
        return Err(error(
            CanonicalParseErrorKind::MissingFrontmatter,
            Some(1),
            "canonical document must start with YAML frontmatter",
        ));
    };

    if trim_line_ending(first_line) != "---" {
        return Err(error(
            CanonicalParseErrorKind::MissingFrontmatter,
            Some(1),
            "canonical document must start with YAML frontmatter",
        ));
    }

    cursor += first_line.len();
    let frontmatter_start = cursor;
    let mut closing_line_number = 1;

    for (offset, line) in lines.enumerate() {
        let line_number = offset + 2;
        let line_start = cursor;
        cursor += line.len();

        if trim_line_ending(line) == "---" {
            closing_line_number = line_number;
            return Ok(FrontmatterSplit {
                frontmatter: &input[frontmatter_start..line_start],
                body: &input[cursor..],
                body_start_line: closing_line_number + 1,
            });
        }
    }

    Err(error(
        CanonicalParseErrorKind::UnterminatedFrontmatter,
        Some(closing_line_number),
        "YAML frontmatter is missing a closing delimiter",
    ))
}

fn parse_directive_attributes(input: &str) -> BTreeMap<String, String> {
    let mut attributes = BTreeMap::new();
    let mut chars = input.char_indices().peekable();

    while let Some((_, ch)) = chars.peek().copied() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }

        let key_start = chars.peek().map(|(index, _)| *index).unwrap_or(input.len());
        while let Some((_, ch)) = chars.peek().copied() {
            if ch == '=' || ch.is_whitespace() {
                break;
            }
            chars.next();
        }
        let key_end = chars.peek().map(|(index, _)| *index).unwrap_or(input.len());
        let key = input[key_start..key_end].trim();

        while let Some((_, ch)) = chars.peek().copied() {
            if ch.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }

        if chars.peek().is_none_or(|(_, ch)| *ch != '=') {
            skip_to_whitespace(&mut chars);
            continue;
        }
        chars.next();

        while let Some((_, ch)) = chars.peek().copied() {
            if ch.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }

        let value = if chars.peek().is_some_and(|(_, ch)| *ch == '"') {
            chars.next();
            parse_quoted_directive_value(&mut chars)
        } else {
            let value_start = chars.peek().map(|(index, _)| *index).unwrap_or(input.len());
            while let Some((_, ch)) = chars.peek().copied() {
                if ch.is_whitespace() {
                    break;
                }
                chars.next();
            }
            let value_end = chars.peek().map(|(index, _)| *index).unwrap_or(input.len());
            input[value_start..value_end].to_string()
        };

        if !key.is_empty() {
            attributes.insert(key.to_string(), value);
        }
    }

    attributes
}

fn parse_quoted_directive_value(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> String {
    let mut value = String::new();

    while let Some((_, ch)) = chars.peek().copied() {
        match ch {
            '"' => {
                chars.next();
                break;
            }
            '\\' => {
                chars.next();
                match chars.next() {
                    Some((_, escaped @ ('"' | '\\'))) => value.push(escaped),
                    Some((_, escaped)) => {
                        value.push('\\');
                        value.push(escaped);
                    }
                    None => value.push('\\'),
                }
            }
            _ => {
                value.push(ch);
                chars.next();
            }
        }
    }

    value
}

fn skip_to_whitespace(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) {
    while let Some((_, ch)) = chars.peek().copied() {
        if ch.is_whitespace() {
            break;
        }
        chars.next();
    }
}

fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn error(
    kind: CanonicalParseErrorKind,
    line: Option<usize>,
    message: impl Into<String>,
) -> CanonicalParseError {
    CanonicalParseError {
        kind,
        line,
        message: message.into(),
    }
}

struct FrontmatterSplit<'a> {
    frontmatter: &'a str,
    body: &'a str,
    body_start_line: usize,
}

#[derive(Debug, Default, Deserialize)]
struct RawFrontmatter {
    loc: Option<RawLocalityMetadata>,
    title: Option<String>,
    #[serde(flatten)]
    properties: FrontmatterProperties,
}

impl RawFrontmatter {
    fn into_frontmatter(self) -> Frontmatter {
        Frontmatter {
            loc: self.loc.map(RawLocalityMetadata::into_metadata),
            title: self.title,
            properties: self.properties,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawLocalityMetadata {
    id: Option<String>,
    #[serde(rename = "type")]
    entity_type: Option<String>,
    parent: Option<String>,
    synced_at: Option<String>,
    remote_edited_at: Option<String>,
}

impl RawLocalityMetadata {
    fn into_metadata(self) -> LocalityMetadata {
        LocalityMetadata {
            id: self.id.filter(|id| !id.is_empty()).map(RemoteId::new),
            entity_type: self
                .entity_type
                .as_deref()
                .filter(|entity_type| !entity_type.is_empty())
                .map(parse_entity_kind),
            raw_entity_type: self.entity_type,
            parent: self
                .parent
                .filter(|parent| !parent.is_empty())
                .map(RemoteId::new),
            synced_at: self.synced_at.filter(|value| !value.is_empty()),
            remote_edited_at: self.remote_edited_at.filter(|value| !value.is_empty()),
        }
    }
}

fn parse_entity_kind(raw: &str) -> EntityKind {
    match raw {
        "page" => EntityKind::Page,
        "database" => EntityKind::Database,
        "directory" => EntityKind::Directory,
        "asset" => EntityKind::Asset,
        other => EntityKind::Unknown(other.to_string()),
    }
}
