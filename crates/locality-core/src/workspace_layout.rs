//! Validated portable mount identity and target components for workspace layout v1.

use std::fmt::{Display, Formatter};

use caseless::Caseless;
use serde::{Deserialize, Deserializer, Serialize};
use unicode_normalization_v16::UnicodeNormalization;

pub const WORKSPACE_LAYOUT_UNICODE_VERSION: (u8, u8, u8) = (16, 0, 0);

const _: () = assert!(unicode_normalization_v16::UNICODE_VERSION.0 == 16);
const _: () = assert!(unicode_normalization_v16::UNICODE_VERSION.1 == 0);
const _: () = assert!(unicode_normalization_v16::UNICODE_VERSION.2 == 0);
const _: () = assert!(caseless::UNICODE_VERSION.0 == 16);
const _: () = assert!(caseless::UNICODE_VERSION.1 == 0);
const _: () = assert!(caseless::UNICODE_VERSION.2 == 0);

/// Stable opaque mount identity used by portable workspace layouts.
///
/// Unlike the legacy [`crate::model::MountId`], this value is validated and is
/// never interpreted as a path. Path separators are therefore permitted.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PortableMountId(String);

impl PortableMountId {
    pub const MAX_UTF8_BYTES: usize = 128;
    pub const MAX_UTF16_UNITS: usize = 128;

    pub fn new(value: impl Into<String>) -> Result<Self, PortableMountIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(PortableMountIdError::Empty);
        }
        if !is_nfc(&value) {
            return Err(PortableMountIdError::NotNfc);
        }
        if value.len() > Self::MAX_UTF8_BYTES {
            return Err(PortableMountIdError::TooManyUtf8Bytes {
                actual: value.len(),
            });
        }
        let utf16_units = value.encode_utf16().count();
        if utf16_units > Self::MAX_UTF16_UNITS {
            return Err(PortableMountIdError::TooManyUtf16Units {
                actual: utf16_units,
            });
        }
        for character in value.chars() {
            if character == '\0' {
                return Err(PortableMountIdError::Nul);
            }
            if character.is_control() {
                return Err(PortableMountIdError::Control(character));
            }
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl Display for PortableMountId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PortableMountId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortableMountIdError {
    Empty,
    NotNfc,
    TooManyUtf8Bytes { actual: usize },
    TooManyUtf16Units { actual: usize },
    Nul,
    Control(char),
}

impl Display for PortableMountIdError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("portable mount ID is empty"),
            Self::NotNfc => formatter.write_str("portable mount ID is not Unicode NFC"),
            Self::TooManyUtf8Bytes { actual } => write!(
                formatter,
                "portable mount ID is {actual} UTF-8 bytes; maximum is {}",
                PortableMountId::MAX_UTF8_BYTES
            ),
            Self::TooManyUtf16Units { actual } => write!(
                formatter,
                "portable mount ID is {actual} UTF-16 code units; maximum is {}",
                PortableMountId::MAX_UTF16_UNITS
            ),
            Self::Nul => formatter.write_str("portable mount ID contains NUL"),
            Self::Control(character) => write!(
                formatter,
                "portable mount ID contains control character U+{:04X}",
                *character as u32
            ),
        }
    }
}

impl std::error::Error for PortableMountIdError {}

/// One validated, portable, root-relative workspace component.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct MountTarget(String);

impl MountTarget {
    pub const MAX_UTF8_BYTES: usize = 120;
    pub const MAX_UTF16_UNITS: usize = 120;

    pub fn new(value: impl Into<String>) -> Result<Self, MountTargetError> {
        let value = value.into();
        if value.is_empty() {
            return Err(MountTargetError::Empty);
        }
        if !is_nfc(&value) {
            return Err(MountTargetError::NotNfc);
        }
        if value.len() > Self::MAX_UTF8_BYTES {
            return Err(MountTargetError::TooManyUtf8Bytes {
                actual: value.len(),
            });
        }
        let utf16_units = value.encode_utf16().count();
        if utf16_units > Self::MAX_UTF16_UNITS {
            return Err(MountTargetError::TooManyUtf16Units {
                actual: utf16_units,
            });
        }
        if matches!(value.as_str(), "." | "..") {
            return Err(MountTargetError::Traversal);
        }
        for character in value.chars() {
            if character == '\0' {
                return Err(MountTargetError::Nul);
            }
            if character.is_control() {
                return Err(MountTargetError::Control(character));
            }
            if matches!(
                character,
                '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'
            ) {
                return Err(MountTargetError::InvalidCharacter(character));
            }
        }
        if value.ends_with(['.', ' ']) {
            return Err(MountTargetError::TrailingDotOrSpace);
        }
        if is_windows_device_name(&value) {
            return Err(MountTargetError::WindowsDeviceName);
        }
        if collision_key(&value) == ".loc" {
            return Err(MountTargetError::ReservedControlComponent);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    /// Unicode 16 full default non-Turkic case fold followed by NFC.
    pub fn collision_key(&self) -> String {
        collision_key(&self.0)
    }
}

impl Display for MountTarget {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MountTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountTargetError {
    Empty,
    NotNfc,
    TooManyUtf8Bytes { actual: usize },
    TooManyUtf16Units { actual: usize },
    Traversal,
    Nul,
    Control(char),
    InvalidCharacter(char),
    TrailingDotOrSpace,
    WindowsDeviceName,
    ReservedControlComponent,
}

impl Display for MountTargetError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("mount target is empty"),
            Self::NotNfc => formatter.write_str("mount target is not Unicode NFC"),
            Self::TooManyUtf8Bytes { actual } => write!(
                formatter,
                "mount target is {actual} UTF-8 bytes; maximum is {}",
                MountTarget::MAX_UTF8_BYTES
            ),
            Self::TooManyUtf16Units { actual } => write!(
                formatter,
                "mount target is {actual} UTF-16 code units; maximum is {}",
                MountTarget::MAX_UTF16_UNITS
            ),
            Self::Traversal => formatter.write_str("mount target cannot be `.` or `..`"),
            Self::Nul => formatter.write_str("mount target contains NUL"),
            Self::Control(character) => write!(
                formatter,
                "mount target contains control character U+{:04X}",
                *character as u32
            ),
            Self::InvalidCharacter(character) => {
                write!(
                    formatter,
                    "mount target contains invalid character `{character}`"
                )
            }
            Self::TrailingDotOrSpace => formatter.write_str("mount target ends in a dot or space"),
            Self::WindowsDeviceName => {
                formatter.write_str("mount target has a reserved Windows device stem")
            }
            Self::ReservedControlComponent => {
                formatter.write_str("mount target is the reserved `.loc` control component")
            }
        }
    }
}

impl std::error::Error for MountTargetError {}

fn is_nfc(value: &str) -> bool {
    value.nfc().eq(value.chars())
}

fn collision_key(value: &str) -> String {
    value.chars().default_case_fold().nfc().collect()
}

fn is_windows_device_name(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| matches!(suffix.as_bytes(), [b'1'..=b'9']))
}
