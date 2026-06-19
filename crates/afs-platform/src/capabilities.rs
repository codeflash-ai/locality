use afs_store::ProjectionMode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformCapabilities {
    pub target_os: String,
    pub default_projection: ProjectionMode,
    pub supported_projections: Vec<ProjectionMode>,
    pub virtual_registration: Option<ProjectionMode>,
    pub supports_daemon_service: bool,
    pub supports_secure_os_credentials: bool,
}

impl PlatformCapabilities {
    pub fn projection_from_cli_value(
        &self,
        value: Option<&str>,
    ) -> Result<ProjectionMode, ProjectionModeError> {
        let projection = match value {
            None | Some("plain-files") => ProjectionMode::PlainFiles,
            Some("macos-file-provider") => ProjectionMode::MacosFileProvider,
            Some("linux-fuse") => ProjectionMode::LinuxFuse,
            Some("windows-cloud-files") => ProjectionMode::WindowsCloudFiles,
            Some(value) => {
                return Err(ProjectionModeError::Unknown {
                    value: value.to_string(),
                    expected: self.projection_usage_options(),
                });
            }
        };

        if self.supported_projections.contains(&projection) {
            Ok(projection)
        } else {
            Err(ProjectionModeError::Unsupported {
                projection: projection_cli_value(&projection).to_string(),
                target_os: self.target_os.clone(),
            })
        }
    }

    pub fn projection_usage_options(&self) -> String {
        self.supported_projections
            .iter()
            .map(projection_cli_value)
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionModeError {
    Unknown {
        value: String,
        expected: String,
    },
    Unsupported {
        projection: String,
        target_os: String,
    },
}

impl ProjectionModeError {
    pub fn message(&self) -> String {
        match self {
            Self::Unknown { expected, .. } => format!("--projection must be {expected}"),
            Self::Unsupported {
                projection,
                target_os,
            } => format!(
                "--projection {projection} is only supported on {}; this binary is running on {target_os}",
                supported_target_for_projection(projection)
            ),
        }
    }
}

pub fn mount_cli_capabilities() -> PlatformCapabilities {
    mount_cli_capabilities_for_target(std::env::consts::OS)
}

pub fn mount_cli_capabilities_for_target(target_os: &str) -> PlatformCapabilities {
    match target_os {
        "macos" => PlatformCapabilities {
            target_os: target_os.to_string(),
            default_projection: ProjectionMode::PlainFiles,
            supported_projections: vec![
                ProjectionMode::PlainFiles,
                ProjectionMode::MacosFileProvider,
            ],
            virtual_registration: Some(ProjectionMode::MacosFileProvider),
            supports_daemon_service: true,
            supports_secure_os_credentials: true,
        },
        "linux" => PlatformCapabilities {
            target_os: target_os.to_string(),
            default_projection: ProjectionMode::PlainFiles,
            supported_projections: vec![ProjectionMode::PlainFiles, ProjectionMode::LinuxFuse],
            virtual_registration: Some(ProjectionMode::LinuxFuse),
            supports_daemon_service: true,
            supports_secure_os_credentials: false,
        },
        "windows" => PlatformCapabilities {
            target_os: target_os.to_string(),
            default_projection: ProjectionMode::PlainFiles,
            supported_projections: vec![ProjectionMode::PlainFiles],
            virtual_registration: None,
            supports_daemon_service: false,
            supports_secure_os_credentials: true,
        },
        _ => PlatformCapabilities {
            target_os: target_os.to_string(),
            default_projection: ProjectionMode::PlainFiles,
            supported_projections: vec![ProjectionMode::PlainFiles],
            virtual_registration: None,
            supports_daemon_service: false,
            supports_secure_os_credentials: false,
        },
    }
}

pub fn projection_cli_value(projection: &ProjectionMode) -> &'static str {
    match projection {
        ProjectionMode::PlainFiles => "plain-files",
        ProjectionMode::MacosFileProvider => "macos-file-provider",
        ProjectionMode::LinuxFuse => "linux-fuse",
        ProjectionMode::WindowsCloudFiles => "windows-cloud-files",
    }
}

fn supported_target_for_projection(projection: &str) -> &'static str {
    match projection {
        "macos-file-provider" => "macOS",
        "linux-fuse" => "Linux",
        "windows-cloud-files" => "Windows after the Cloud Files provider is implemented",
        _ => "this platform",
    }
}

#[cfg(test)]
mod tests {
    use super::mount_cli_capabilities_for_target;
    use afs_store::ProjectionMode;

    #[test]
    fn windows_cli_supports_plain_files_only_for_now() {
        let capabilities = mount_cli_capabilities_for_target("windows");

        assert_eq!(capabilities.default_projection, ProjectionMode::PlainFiles);
        assert_eq!(capabilities.projection_usage_options(), "plain-files");
        assert_eq!(
            capabilities
                .projection_from_cli_value(Some("windows-cloud-files"))
                .expect_err("cloud files is not wired yet")
                .message(),
            "--projection windows-cloud-files is only supported on Windows after the Cloud Files provider is implemented; this binary is running on windows"
        );
    }

    #[test]
    fn macos_cli_supports_file_provider() {
        let capabilities = mount_cli_capabilities_for_target("macos");

        assert_eq!(
            capabilities
                .projection_from_cli_value(Some("macos-file-provider"))
                .expect("file provider projection"),
            ProjectionMode::MacosFileProvider
        );
        assert_eq!(
            capabilities.projection_usage_options(),
            "plain-files|macos-file-provider"
        );
    }

    #[test]
    fn linux_cli_supports_fuse() {
        let capabilities = mount_cli_capabilities_for_target("linux");

        assert_eq!(
            capabilities
                .projection_from_cli_value(Some("linux-fuse"))
                .expect("linux fuse projection"),
            ProjectionMode::LinuxFuse
        );
        assert_eq!(
            capabilities.projection_usage_options(),
            "plain-files|linux-fuse"
        );
    }
}
